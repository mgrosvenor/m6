/// bench_coldstart — external black-box cold-start full-page-load benchmark.
///
/// Treats m6-http as a pure black box.  Each sample opens a brand-new
/// connection, fetches the HTML page and all linked assets, then tears down.
/// Measures wall-clock time from first connect to last asset byte.
///
/// H1: TcpStream + rustls, HTTP/1.1 keep-alive (one TCP conn per page load).
/// H2: tokio + tokio-rustls + h2 crate (one TLS+H2 conn per page load).
/// H3: quiche (one QUIC conn per page load).
///
/// Flags:
///   --addr HOST:PORT    target  (default 127.0.0.1:8443)
///   --html PATH         HTML page to fetch  (default /)
///   --n N               samples per protocol  (default 10000)
///   --warmup N          warmup requests before measurement  (default 100)
///   --skip-verify       skip TLS certificate verification
///   --http11-only / --http2-only / --http3-only
///   --out-dir DIR       SVG output directory  (default .)

use std::io::{self, Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::Rng;
use rustls::ClientConfig;
use rustls::pki_types::{ServerName, CertificateDer, UnixTime};

// ── CLI ───────────────────────────────────────────────────────────────────────

struct Args {
    addr:        String,
    html:        String,
    n:           usize,
    warmup:      usize,
    skip_verify: bool,
    http11:      bool,
    http2:       bool,
    http3:       bool,
    out_dir:     String,
}

impl Args {
    fn parse() -> Self {
        let mut a = Args {
            addr:        "127.0.0.1:8443".into(),
            html:        "/".into(),
            n:           10_000,
            warmup:      100,
            skip_verify: false,
            http11:      true,
            http2:       true,
            http3:       true,
            out_dir:     ".".into(),
        };
        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < raw.len() {
            match raw[i].as_str() {
                "--addr"        => { i += 1; a.addr        = raw[i].clone(); }
                "--html"        => { i += 1; a.html        = raw[i].clone(); }
                "--n"           => { i += 1; a.n           = raw[i].parse().expect("--n"); }
                "--warmup"      => { i += 1; a.warmup      = raw[i].parse().expect("--warmup"); }
                "--out-dir"     => { i += 1; a.out_dir     = raw[i].clone(); }
                "--skip-verify" => { a.skip_verify = true; }
                "--http11-only" => { a.http11 = true;  a.http2 = false; a.http3 = false; }
                "--http2-only"  => { a.http11 = false; a.http2 = true;  a.http3 = false; }
                "--http3-only"  => { a.http11 = false; a.http2 = false; a.http3 = true;  }
                other => { eprintln!("unknown flag: {other}"); std::process::exit(1); }
            }
            i += 1;
        }
        a
    }
}

// ── TLS helpers ───────────────────────────────────────────────────────────────

#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>],
        _: &ServerName<'_>, _: &[u8], _: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(&self, _: &[u8], _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms.supported_schemes()
    }
}

fn make_rustls_config(skip_verify: bool) -> Arc<ClientConfig> {
    if skip_verify {
        Arc::new(ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth())
    } else {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Arc::new(ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth())
    }
}

#[allow(dead_code)]
fn make_rustls_config_h2(skip_verify: bool) -> Arc<ClientConfig> {
    if skip_verify {
        Arc::new(ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth())
    } else {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let mut cfg = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        cfg.alpn_protocols = vec![b"h2".to_vec()];
        Arc::new(cfg)
    }
}

// ── Statistics ────────────────────────────────────────────────────────────────

struct Sample {
    connect_us:  f64,   // TCP/QUIC connect
    html_us:     f64,   // start → HTML last byte
    total_us:    f64,   // start → all assets last byte
}

struct BoxStats {
    label:  String,
    n:      usize,
    p5:     f64,
    p25:    f64,
    p50:    f64,
    p75:    f64,
    p95:    f64,
    mean:   f64,
    stddev: f64,
    max:    f64,
}

impl BoxStats {
    fn from_vec(label: impl Into<String>, mut v: Vec<f64>) -> Self {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = v.len();
        let pct = |p: f64| {
            let idx = ((p / 100.0) * (n - 1) as f64).round() as usize;
            v[idx.min(n - 1)]
        };
        let mean   = v.iter().sum::<f64>() / n as f64;
        let stddev = (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n as f64).sqrt();
        BoxStats {
            label: label.into(), n,
            p5: pct(5.0), p25: pct(25.0), p50: pct(50.0),
            p75: pct(75.0), p95: pct(95.0),
            mean, stddev, max: *v.last().unwrap(),
        }
    }
}

fn print_table(stats: &[BoxStats]) {
    if stats.is_empty() { return; }
    println!("{:<32} {:>7} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "metric", "n", "p5", "p25", "p50", "p75", "p95", "mean", "stddev", "max");
    println!("{}", "-".repeat(106));
    for s in stats {
        println!("{:<32} {:>7} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>8.1} {:>8.1}  µs",
            s.label, s.n, s.p5, s.p25, s.p50, s.p75, s.p95, s.mean, s.stddev, s.max);
    }
    println!();
}

// ── SVG box-and-whisker chart ─────────────────────────────────────────────────

fn write_svg(stats: &[BoxStats], title: &str, path: &str) -> std::io::Result<()> {
    if stats.is_empty() { return Ok(()); }

    let label_w: f64 = 200.0;
    let plot_w:  f64 = 900.0;
    let row_h:   f64 = 44.0;
    let top:     f64 = 60.0;
    let bottom:  f64 = 60.0;
    let box_h:   f64 = 20.0;
    let canvas_w = (label_w + plot_w + 40.0) as u32;
    let canvas_h = (top + row_h * stats.len() as f64 + bottom) as u32;

    let x_max = stats.iter().map(|s| s.p95).fold(0.0_f64, f64::max) * 1.15;
    let x_max = if x_max == 0.0 { 1.0 } else { x_max };
    let to_x  = |v: f64| label_w + (v / x_max) * plot_w;

    macro_rules! el {
        ($svg:expr, $($arg:tt)*) => { $svg.push_str(&format!($($arg)*)); $svg.push('\n'); }
    }

    let mut svg = String::with_capacity(32_768);
    el!(svg, "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" \
              font-family=\"monospace\" font-size=\"13\">", canvas_w, canvas_h);
    el!(svg, "<rect width=\"{}\" height=\"{}\" fill=\"{}\"/>", canvas_w, canvas_h, "#f8f9fa");
    el!(svg, "<text x=\"{:.1}\" y=\"30\" font-size=\"16\" font-weight=\"bold\" \
              fill=\"{}\" text-anchor=\"middle\">{}</text>",
        canvas_w as f64 / 2.0, "#222", title);

    let n_ticks = 5usize;
    let axis_y  = top + row_h * stats.len() as f64;
    el!(svg, "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" \
              stroke=\"{}\" stroke-width=\"1\"/>",
        to_x(0.0), axis_y, to_x(x_max), axis_y, "#aaa");
    for t in 0..=n_ticks {
        let v  = x_max * t as f64 / n_ticks as f64;
        let tx = to_x(v);
        el!(svg, "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" \
                  stroke=\"{}\" stroke-width=\"1\"/>",
            tx, axis_y, tx, axis_y + 5.0, "#aaa");
        el!(svg, "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" \
                  fill=\"{}\">{:.0}µs</text>", tx, axis_y + 18.0, "#555", v);
        el!(svg, "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" \
                  stroke=\"{}\" stroke-width=\"1\" stroke-dasharray=\"4,4\"/>",
            tx, top, tx, axis_y, "#ddd");
    }
    el!(svg, "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"{}\" \
              font-size=\"12\">Latency (µs)</text>",
        label_w + plot_w / 2.0, axis_y + 36.0, "#555");

    for (i, s) in stats.iter().enumerate() {
        let cy        = top + row_h * i as f64 + row_h / 2.0;
        let y_top     = cy - box_h / 2.0;
        let y_bot     = cy + box_h / 2.0;
        if i % 2 == 0 {
            el!(svg, "<rect x=\"0\" y=\"{:.1}\" width=\"{}\" height=\"{:.1}\" \
                      fill=\"{}\" opacity=\"0.5\"/>",
                top + row_h * i as f64, canvas_w, row_h, "#eef0f4");
        }
        let lbl = s.label.split('/').last().unwrap_or(&s.label);
        el!(svg, "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\" fill=\"{}\" \
                  dominant-baseline=\"middle\">{}</text>",
            label_w - 8.0, cy, "#333", lbl);

        // whisker
        el!(svg, "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" \
                  stroke=\"{}\" stroke-width=\"2\"/>",
            to_x(s.p5), cy, to_x(s.p95), cy, "#6699cc");
        for &v in &[s.p5, s.p95] {
            let vx = to_x(v);
            el!(svg, "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" \
                      stroke=\"{}\" stroke-width=\"2\"/>", vx, y_top, vx, y_bot, "#6699cc");
        }
        // IQR box
        let bx = to_x(s.p25);
        let bw = (to_x(s.p75) - bx).max(1.0);
        el!(svg, "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" \
                  fill=\"{}\" opacity=\"0.7\" rx=\"2\"/>", bx, y_top, bw, box_h, "#4488cc");
        // median
        let mx = to_x(s.p50);
        el!(svg, "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" \
                  stroke=\"{}\" stroke-width=\"2.5\"/>", mx, y_top, mx, y_bot, "#fff");
        // mean diamond
        let mnx = to_x(s.mean);
        let d   = 5.0_f64;
        el!(svg, "<polygon points=\"{:.1},{:.1} {:.1},{:.1} {:.1},{:.1} {:.1},{:.1}\" \
                  fill=\"{}\" opacity=\"0.9\"/>",
            mnx-d, cy, mnx, cy-d, mnx+d, cy, mnx, cy+d, "#ff6600");
        // labels
        el!(svg, "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"11\" fill=\"{}\" \
                  dominant-baseline=\"middle\" text-anchor=\"middle\">p50={:.0}</text>",
            (to_x(s.p25) + to_x(s.p75)) / 2.0, cy, "#ddf", s.p50);
        el!(svg, "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"10\" fill=\"{}\" \
                  text-anchor=\"middle\">{:.0}</text>",
            to_x(s.p5),  y_top - 2.0, "#666", s.p5);
        el!(svg, "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"10\" fill=\"{}\" \
                  text-anchor=\"middle\">{:.0}</text>",
            to_x(s.p95), y_top - 2.0, "#666", s.p95);
    }

    // legend
    let lx = label_w + plot_w + 5.0;
    let ly = top + 10.0;
    el!(svg, "<rect x=\"{:.0}\" y=\"{:.0}\" width=\"14\" height=\"14\" \
              fill=\"{}\" opacity=\"0.7\"/>", lx, ly, "#4488cc");
    el!(svg, "<text x=\"{:.0}\" y=\"{:.0}\" font-size=\"11\" fill=\"{}\" \
              dominant-baseline=\"middle\">IQR (p25-p75)</text>", lx+18.0, ly+7.0, "#444");
    el!(svg, "<line x1=\"{:.0}\" y1=\"{:.0}\" x2=\"{:.0}\" y2=\"{:.0}\" \
              stroke=\"{}\" stroke-width=\"2\"/>", lx, ly+24.0, lx+14.0, ly+24.0, "#6699cc");
    el!(svg, "<text x=\"{:.0}\" y=\"{:.0}\" font-size=\"11\" fill=\"{}\" \
              dominant-baseline=\"middle\">p5-p95 whiskers</text>", lx+18.0, ly+24.0, "#444");
    let dlx = lx+10.0; let dly = ly+38.0;
    el!(svg, "<polygon points=\"{:.1},{:.1} {:.1},{:.1} {:.1},{:.1} {:.1},{:.1}\" \
              fill=\"{}\" opacity=\"0.9\"/>",
        dlx-5.0, dly, dlx, dly-5.0, dlx+5.0, dly, dlx, dly+5.0, "#ff6600");
    el!(svg, "<text x=\"{:.0}\" y=\"{:.0}\" font-size=\"11\" fill=\"{}\" \
              dominant-baseline=\"middle\">mean</text>", lx+18.0, dly, "#444");
    el!(svg, "<line x1=\"{:.0}\" y1=\"{:.0}\" x2=\"{:.0}\" y2=\"{:.0}\" \
              stroke=\"{}\" stroke-width=\"2.5\"/>", lx, ly+52.0, lx+14.0, ly+52.0, "#555");
    el!(svg, "<text x=\"{:.0}\" y=\"{:.0}\" font-size=\"11\" fill=\"{}\" \
              dominant-baseline=\"middle\">median (p50)</text>", lx+18.0, ly+52.0, "#444");

    svg.push_str("\n</svg>\n");
    std::fs::write(path, &svg)
}

// ── Asset extraction ──────────────────────────────────────────────────────────
// Handles both quoted (href="/foo") and unquoted (href=/foo) attribute values
// produced by m6-html's minifier.

fn extract_assets(html: &[u8]) -> Vec<String> {
    let mut assets = Vec::new();
    let mut i = 0;
    while i < html.len() {
        let skip = if html[i..].starts_with(b"href=") { 5 }
                   else if html[i..].starts_with(b"src=") { 4 }
                   else { i += 1; continue; };
        i += skip;
        if i >= html.len() { break; }
        let quoted = html[i] == b'"' || html[i] == b'\'';
        let close  = if quoted { let q = html[i]; i += 1; q } else { b' ' };
        let start  = i;
        while i < html.len() {
            let c = html[i];
            if quoted  { if c == close { break; } }
            else       { if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r'
                            || c == b'>' || c == b'"' || c == b'\'' { break; } }
            i += 1;
        }
        if i > start {
            let val = std::str::from_utf8(&html[start..i]).unwrap_or("").trim();
            if val.starts_with('/') && (val.ends_with(".css") || val.ends_with(".js")
                || val.ends_with(".png") || val.ends_with(".jpg") || val.ends_with(".svg")
                || val.ends_with(".woff2") || val.ends_with(".woff") || val.ends_with(".ttf"))
            {
                assets.push(val.to_string());
            }
        }
    }
    assets.sort();
    assets.dedup();
    assets
}

// ── HTTP/1.1 client ───────────────────────────────────────────────────────────
// Cold-start model: new TCP+TLS per resource (HTML + each asset).  This is
// conservative — no keep-alive — which clearly shows the per-resource setup
// cost H1 pays vs H2/H3 multiplexing.  Uses the same TLS handshake pattern
// that bench_detail uses (proven to work with the NoVerify config).

fn h1_single_get(addr: &str, path: &str, tls_cfg: Arc<ClientConfig>)
    -> anyhow::Result<Vec<u8>>
{
    let stream = TcpStream::connect(addr)?;
    stream.set_nodelay(true)?;
    let server_name: ServerName<'static> = "localhost".try_into().unwrap();
    let mut conn = rustls::ClientConnection::new(tls_cfg, server_name)?;

    // Drive TLS handshake explicitly
    while conn.is_handshaking() {
        loop {
            match conn.write_tls(&mut &stream) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        if conn.is_handshaking() {
            match conn.read_tls(&mut &stream) {
                Ok(0) => anyhow::bail!("TLS closed during handshake"),
                Ok(_) => { conn.process_new_packets().map_err(|e| anyhow::anyhow!("{e}"))?; }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => unsafe {
                    let mut pfd = libc::pollfd {
                        fd: stream.as_raw_fd(), events: libc::POLLIN, revents: 0,
                    };
                    libc::poll(&mut pfd, 1, 5000);
                },
                Err(e) => return Err(e.into()),
            }
        }
    }

    // Send request and read full response
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    {
        let mut s = &stream;
        rustls::Stream::new(&mut conn, &mut s).write_all(req.as_bytes())?;
    }
    let mut resp = Vec::new();
    {
        let mut s = &stream;
        match rustls::Stream::new(&mut conn, &mut s).read_to_end(&mut resp) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof && !resp.is_empty() => {}
            Err(e) => return Err(e.into()),
        }
    }

    // Strip HTTP headers, return body
    if let Some(pos) = resp.windows(4).position(|w| w == b"\r\n\r\n") {
        Ok(resp[pos + 4..].to_vec())
    } else {
        Ok(resp)
    }
}

fn bench_h1(
    addr: &str, html_path: &str, n: usize, warmup: usize,
    tls_cfg: Arc<ClientConfig>,
) -> anyhow::Result<Vec<Sample>> {
    // Discover assets
    let html_body = h1_single_get(addr, html_path, Arc::clone(&tls_cfg))?;
    let assets = extract_assets(&html_body);

    // Warm cache
    for _ in 0..warmup {
        h1_single_get(addr, html_path, Arc::clone(&tls_cfg))?;
        for asset in &assets {
            h1_single_get(addr, asset, Arc::clone(&tls_cfg))?;
        }
    }

    let mut samples = Vec::with_capacity(n);
    for _ in 0..n {
        let t_start = Instant::now();
        // First resource: HTML (new TCP+TLS)
        h1_single_get(addr, html_path, Arc::clone(&tls_cfg))?;
        let t_html = Instant::now();
        // Subsequent resources: assets (each gets its own TCP+TLS)
        for asset in &assets {
            h1_single_get(addr, asset, Arc::clone(&tls_cfg))?;
        }
        let t_done = Instant::now();
        let us = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1_000_000.0;
        // connect_us: time to establish the first connection (HTML TCP+TLS+request+response)
        // reported as the html_done time since H1 connect is inseparable from the request
        samples.push(Sample {
            connect_us: us(t_start, t_html),
            html_us:    us(t_start, t_html),
            total_us:   us(t_start, t_done),
        });
    }
    Ok(samples)
}

// ── HTTP/2 client (tokio + h2 crate) ─────────────────────────────────────────
// Uses the standard h2 crate with tokio-rustls for TLS.  One new H2 connection
// is opened per page-load sample; all requests are issued on that connection.

use tokio::net::TcpStream as TokioTcpStream;
use tokio_rustls::TlsConnector;
use h2::client as h2client;
use http::{Request, Method};

async fn h2_cold_page_load_async(
    addr: &str,
    html_path: &str,
    assets: &[String],
    tls_cfg: Arc<ClientConfig>,
) -> anyhow::Result<(f64, f64, f64)> {
    let t_start = Instant::now();

    let tcp = TokioTcpStream::connect(addr).await?;
    tcp.set_nodelay(true)?;

    let connector = TlsConnector::from(Arc::new(
        // h2 ALPN must be set on the rustls config
        {
            let mut c = (*tls_cfg).clone();
            c.alpn_protocols = vec![b"h2".to_vec()];
            c
        }
    ));
    let domain = ServerName::try_from("localhost")
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .to_owned();
    let tls = connector.connect(domain, tcp).await?;
    let t_conn = Instant::now();

    let (mut client, conn) = h2client::handshake(tls).await?;
    tokio::spawn(async move { conn.await.ok(); });

    let send_get = |c: &mut h2client::SendRequest<bytes::Bytes>, path: &str| {
        let req = Request::builder()
            .method(Method::GET)
            .uri(format!("https://localhost{path}"))
            .header("host", "localhost")
            .body(())
            .unwrap();
        c.send_request(req, true)
    };

    let drain = |resp: h2::client::ResponseFuture| async move {
        let resp = resp.await?;
        let mut body = resp.into_body();
        while let Some(chunk) = body.data().await {
            let chunk = chunk?;
            let _ = body.flow_control().release_capacity(chunk.len());
        }
        anyhow::Ok(())
    };

    let (resp, _) = send_get(&mut client, html_path)?;
    drain(resp).await?;
    let t_html = Instant::now();

    for asset in assets {
        let (resp, _) = send_get(&mut client, asset)?;
        drain(resp).await?;
    }
    let t_done = Instant::now();

    let us = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1_000_000.0;
    Ok((us(t_start, t_conn), us(t_start, t_html), us(t_start, t_done)))
}

fn bench_h2(
    addr: &str, html_path: &str, n: usize, warmup: usize,
    tls_cfg: Arc<ClientConfig>,
) -> anyhow::Result<Vec<Sample>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all().build()?;

    // Discover assets
    let html_body = rt.block_on(async {
        let tcp = TokioTcpStream::connect(addr).await?;
        tcp.set_nodelay(true)?;
        let mut c2 = (*tls_cfg).clone();
        c2.alpn_protocols = vec![b"h2".to_vec()];
        let connector = TlsConnector::from(Arc::new(c2));
        let domain = ServerName::try_from("localhost")
            .map_err(|e| anyhow::anyhow!("{e}"))?.to_owned();
        let tls = connector.connect(domain, tcp).await?;
        let (mut client, conn) = h2client::handshake(tls).await?;
        tokio::spawn(async move { conn.await.ok(); });
        let req = Request::builder()
            .method(Method::GET)
            .uri(format!("https://localhost{html_path}"))
            .header("host", "localhost")
            .body(()).unwrap();
        let (resp, _) = client.send_request(req, true)?;
        let resp = resp.await?;
        let mut body = resp.into_body();
        let mut bytes = Vec::new();
        while let Some(chunk) = body.data().await {
            let chunk = chunk?;
            let _ = body.flow_control().release_capacity(chunk.len());
            bytes.extend_from_slice(&chunk);
        }
        anyhow::Ok(bytes)
    })?;
    let assets = extract_assets(&html_body);

    // Warm cache
    for _ in 0..warmup {
        rt.block_on(h2_cold_page_load_async(
            addr, html_path, &assets, Arc::clone(&tls_cfg)))?;
    }

    let mut samples = Vec::with_capacity(n);
    for _ in 0..n {
        let (connect, html, total) = rt.block_on(h2_cold_page_load_async(
            addr, html_path, &assets, Arc::clone(&tls_cfg)))?;
        samples.push(Sample { connect_us: connect, html_us: html, total_us: total });
    }
    Ok(samples)
}

// ── HTTP/3 client (quiche) ────────────────────────────────────────────────────
// One new QUIC connection per page-load sample.  All requests issued on the
// same QUIC connection (no reconnect within a sample for normal page sizes).

const H3_MAX_STREAMS: usize = 90;

fn make_quiche_cfg(skip_verify: bool) -> quiche::Config {
    let mut cfg = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
    if skip_verify { cfg.verify_peer(false); }
    cfg.set_application_protos(quiche::h3::APPLICATION_PROTOCOL).unwrap();
    cfg.set_max_idle_timeout(5_000);
    cfg.set_max_recv_udp_payload_size(1_350);
    cfg.set_max_send_udp_payload_size(1_350);
    cfg.set_initial_max_data(16 * 1024 * 1024);
    cfg.set_initial_max_stream_data_bidi_local(1 * 1024 * 1024);
    cfg.set_initial_max_stream_data_bidi_remote(1 * 1024 * 1024);
    cfg.set_initial_max_stream_data_uni(1 * 1024 * 1024);
    cfg.set_initial_max_streams_bidi(128);
    cfg.set_initial_max_streams_uni(128);
    cfg.set_disable_active_migration(true);
    cfg
}

fn new_scid() -> quiche::ConnectionId<'static> {
    let mut scid = [0u8; quiche::MAX_CONN_ID_LEN];
    rand::thread_rng().fill(&mut scid);
    quiche::ConnectionId::from_vec(scid.to_vec())
}

fn quic_flush(conn: &mut quiche::Connection, udp: &UdpSocket) {
    let mut out = [0u8; 1_350];
    loop {
        match conn.send(&mut out) {
            Ok((n, _)) => { udp.send(&out[..n]).ok(); }
            Err(quiche::Error::Done) => break,
            Err(e) => { eprintln!("quic_flush: {e}"); break; }
        }
    }
}

/// Open a new QUIC+H3 connection. Returns (conn, h3, udp, quic_hs_us).
fn h3_connect(addr: &str, cfg: &mut quiche::Config)
    -> anyhow::Result<(quiche::Connection, quiche::h3::Connection, UdpSocket, f64)>
{
    let t0  = Instant::now();
    let udp = UdpSocket::bind("0.0.0.0:0")?;
    udp.connect(addr)?;
    let local = udp.local_addr()?;
    let peer  = addr.parse()?;
    let scid  = new_scid();
    let mut conn = quiche::connect(Some("localhost"), &scid, local, peer, cfg)?;
    quic_flush(&mut conn, &udp);

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut buf = [0u8; 65_536];
    loop {
        if conn.is_established() { break; }
        if Instant::now() > deadline { anyhow::bail!("H3 handshake timeout"); }
        udp.set_read_timeout(Some(Duration::from_millis(200)))?;
        match udp.recv(&mut buf) {
            Ok(n) => {
                conn.recv(&mut buf[..n], quiche::RecvInfo { from: peer, to: local })?;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock
                   || e.kind() == io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e.into()),
        }
        quic_flush(&mut conn, &udp);
    }
    let t_quic = Instant::now();

    let h3_cfg = quiche::h3::Config::new()?;
    let h3 = quiche::h3::Connection::with_transport(&mut conn, &h3_cfg)?;
    Ok((conn, h3, udp, (t_quic - t0).as_secs_f64() * 1_000_000.0))
}

/// Issue one GET over an existing QUIC+H3 connection. Returns body bytes.
fn h3_get(
    conn: &mut quiche::Connection,
    h3:   &mut quiche::h3::Connection,
    udp:  &UdpSocket,
    path: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let headers = [
        quiche::h3::Header::new(b":method",    b"GET"),
        quiche::h3::Header::new(b":path",      path),
        quiche::h3::Header::new(b":scheme",    b"https"),
        quiche::h3::Header::new(b":authority", b"localhost"),
    ];
    let sid = h3.send_request(conn, &headers, true)?;
    quic_flush(conn, udp);

    let deadline = Instant::now() + Duration::from_secs(5);
    let peer     = udp.peer_addr()?;
    let local    = udp.local_addr()?;
    let mut buf  = [0u8; 65_536];
    let mut body = Vec::new();

    loop {
        if Instant::now() > deadline { anyhow::bail!("H3 response timeout"); }
        quic_flush(conn, udp);
        udp.set_read_timeout(Some(Duration::from_millis(200)))?;
        match udp.recv(&mut buf) {
            Ok(n) => {
                conn.recv(&mut buf[..n], quiche::RecvInfo { from: peer, to: local })?;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock
                   || e.kind() == io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e.into()),
        }
        quic_flush(conn, udp);

        loop {
            match h3.poll(conn) {
                Ok((_, quiche::h3::Event::Data)) => {
                    let mut tmp = [0u8; 65_536];
                    while let Ok(n) = h3.recv_body(conn, sid, &mut tmp) {
                        body.extend_from_slice(&tmp[..n]);
                    }
                }
                Ok((_, quiche::h3::Event::Finished)) => return Ok(body),
                Ok(_) => {}
                Err(quiche::h3::Error::Done) => break,
                Err(e) => return Err(anyhow::anyhow!("H3 poll: {e}")),
            }
        }
    }
}

/// Measure one cold-start H3 page load. Returns (connect_us, html_us, total_us).
fn h3_cold_page_load(
    addr: &str,
    html_path: &str,
    assets: &[String],
    cfg: &mut quiche::Config,
) -> anyhow::Result<(f64, f64, f64)> {
    let t_start = Instant::now();
    let (mut conn, mut h3, udp, connect_us) = h3_connect(addr, cfg)?;
    let mut reqs = 0usize;

    h3_get(&mut conn, &mut h3, &udp, html_path.as_bytes())?;
    reqs += 1;
    let t_html = Instant::now();

    for asset in assets {
        if reqs >= H3_MAX_STREAMS {
            // Reconnect between assets if stream limit hit (rare for normal pages).
            let (c, h, u, _) = h3_connect(addr, cfg)?;
            conn = c; h3 = h; _ = u; reqs = 0;
        }
        h3_get(&mut conn, &mut h3, &udp, asset.as_bytes())?;
        reqs += 1;
    }
    let t_done = Instant::now();

    let us = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1_000_000.0;
    Ok((connect_us, us(t_start, t_html), us(t_start, t_done)))
}

fn bench_h3(
    addr: &str, html_path: &str, n: usize, warmup: usize,
    skip_verify: bool,
) -> anyhow::Result<Vec<Sample>> {
    let mut cfg = make_quiche_cfg(skip_verify);

    // Discover assets
    let (mut conn, mut h3, udp, _) = h3_connect(addr, &mut cfg)?;
    let html_body = h3_get(&mut conn, &mut h3, &udp, html_path.as_bytes())?;
    let assets = extract_assets(&html_body);

    // Warm cache
    for _ in 0..warmup {
        h3_cold_page_load(addr, html_path, &assets, &mut cfg)?;
    }

    let mut samples = Vec::with_capacity(n);
    for _ in 0..n {
        let (connect, html, total) =
            h3_cold_page_load(addr, html_path, &assets, &mut cfg)?;
        samples.push(Sample { connect_us: connect, html_us: html, total_us: total });
    }
    Ok(samples)
}

// ── main ──────────────────────────────────────────────────────────────────────

fn run_proto(
    proto:    &str,
    samples:  Vec<Sample>,
    out_dir:  &str,
) {
    let n = samples.len();
    let connect = BoxStats::from_vec(format!("{proto}/connect"),
        samples.iter().map(|s| s.connect_us).collect());
    let html    = BoxStats::from_vec(format!("{proto}/html_done"),
        samples.iter().map(|s| s.html_us).collect());
    let total   = BoxStats::from_vec(format!("{proto}/total"),
        samples.iter().map(|s| s.total_us).collect());

    let stats = vec![connect, html, total];
    print_table(&stats);

    let svg_path = format!("{out_dir}/m6_coldstart_{}.svg",
        proto.to_lowercase().replace('/', ""));
    match write_svg(&stats,
        &format!("{proto} Cold-Start Full-Page Load  (n={n}, µs)"),
        &svg_path)
    {
        Ok(()) => println!("Chart: {svg_path}"),
        Err(e) => eprintln!("SVG write error: {e}"),
    }
}

fn main() {
    // Ensure ring is the active crypto provider.  tokio-rustls may pull in
    // aws-lc-rs as well, which causes rustls to refuse to auto-select; an
    // explicit install avoids the ambiguity.  .ok() makes it idempotent.
    rustls::crypto::ring::default_provider().install_default().ok();

    let args = Args::parse();

    println!("m6-bench-coldstart  target={}  html={}  n={}  warmup={}  skip-verify={}",
             args.addr, args.html, args.n, args.warmup, args.skip_verify);
    println!("Cold-start: new connection per page load (HTML + all linked assets)");
    println!("{}", "=".repeat(106));

    if args.http11 {
        println!("\n[HTTP/1.1] Cold-start full-page load (n={}, warmup={})",
                 args.n, args.warmup);
        let tls_cfg = make_rustls_config(args.skip_verify);
        match bench_h1(&args.addr, &args.html, args.n, args.warmup, tls_cfg) {
            Ok(s)  => run_proto("HTTP/1.1", s, &args.out_dir),
            Err(e) => eprintln!("H1 bench error: {e}"),
        }
    }

    if args.http2 {
        println!("\n[HTTP/2] Cold-start full-page load (n={}, warmup={})",
                 args.n, args.warmup);
        let tls_cfg = make_rustls_config(args.skip_verify);
        match bench_h2(&args.addr, &args.html, args.n, args.warmup, tls_cfg) {
            Ok(s)  => run_proto("HTTP/2", s, &args.out_dir),
            Err(e) => eprintln!("H2 bench error: {e}"),
        }
    }

    if args.http3 {
        println!("\n[HTTP/3] Cold-start full-page load (n={}, warmup={})",
                 args.n, args.warmup);
        match bench_h3(&args.addr, &args.html, args.n, args.warmup, args.skip_verify) {
            Ok(s)  => run_proto("HTTP/3", s, &args.out_dir),
            Err(e) => eprintln!("H3 bench error: {e}"),
        }
    }
}

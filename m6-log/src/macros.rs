//! The logging macros: an optional target, typed fields, then the message.
//!
//! `error!`, `warn!`, `info!`, `debug!` and `trace!` take `target: "..."`
//! first, then `key = value`, `key = %display`, `key = ?debug` fields, then the
//! message. The message is a format string with arguments.
//!
//! Each macro compares the level before it builds anything, and a line whose
//! level is off costs one load and one compare.

/// Write a line at `error`.
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => { $crate::__log_line!($crate::Level::Error, $($arg)*) };
}

/// Write a line at `warn`.
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::__log_line!($crate::Level::Warn, $($arg)*) };
}

/// Write a line at `info`.
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::__log_line!($crate::Level::Info, $($arg)*) };
}

/// Write a line at `debug`.
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => { $crate::__log_line!($crate::Level::Debug, $($arg)*) };
}

/// Write a line at `trace`.
#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => { $crate::__log_line!($crate::Level::Trace, $($arg)*) };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __log_line {
    ($level:expr, target: $target:expr, $($rest:tt)*) => {
        $crate::__log_build!($level, $target, $($rest)*)
    };
    ($level:expr, $($rest:tt)*) => {
        $crate::__log_build!($level, module_path!(), $($rest)*)
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __log_build {
    ($level:expr, $target:expr, $($rest:tt)*) => {
        if $crate::__level_enabled($level)
            && $crate::__with_operational(|sink| {
                let line = $crate::Line::empty(sink, $level);
                let line = $crate::Line::target(line, $target);
                $crate::__log_fields!(line, $($rest)*);
                $crate::__flush_if_urgent($level, sink);
            })
        {
            $crate::__pulse_record();
        }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __log_fields {
    ($line:expr,) => { $crate::Line::end($line) };
    ($line:expr) => { $crate::Line::end($line) };
    ($line:expr, $key:ident = %$val:expr, $($rest:tt)*) => {
        $crate::__log_fields!($crate::Line::display($line, stringify!($key), &$val), $($rest)*)
    };
    ($line:expr, $key:ident = ?$val:expr, $($rest:tt)*) => {
        $crate::__log_fields!($crate::Line::debug($line, stringify!($key), &$val), $($rest)*)
    };
    ($line:expr, $key:ident = $val:expr, $($rest:tt)*) => {
        $crate::__log_fields!($crate::Line::field($line, stringify!($key), $val), $($rest)*)
    };
    ($line:expr, $key:ident, $($rest:tt)*) => {
        $crate::__log_fields!($crate::Line::field($line, stringify!($key), $key), $($rest)*)
    };
    ($line:expr, $($message:tt)+) => {
        $crate::Line::end($crate::Line::message($line, format_args!($($message)+)))
    };
}

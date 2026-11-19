SITE := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
M6_MD ?= m6-md
POSTS_SRC := $(wildcard $(SITE)content/posts/*.md)

.PHONY: build reload

build: $(SITE)data/posts.json

$(SITE)data/posts.json: $(POSTS_SRC)
	$(M6_MD) $(SITE)content/posts/ --output $(SITE)data/posts.json

reload: $(SITE)data/posts.json
	touch $(SITE)site.toml

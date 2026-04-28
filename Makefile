V=@

PDATA_TOOLS:=\
	target/release/pdata_tools
CARGO_FLAGS:=$(if $(DISABLE_THIN_MIGRATE),--no-default-features)

$(PDATA_TOOLS):
	$(V) cargo build --release $(CARGO_FLAGS)

PREFIX:=/usr
BINDIR:=$(DESTDIR)$(PREFIX)/sbin
DATADIR:=$(DESTDIR)$(PREFIX)/share
MANPATH:=$(DATADIR)/man

STRIP:=strip
INSTALL:=install
INSTALL_DIR = $(INSTALL) -m 755 -d
INSTALL_PROGRAM = $(INSTALL) -m 755
INSTALL_DATA = $(INSTALL) -p -m 644

.SUFFIXES: .txt .8

%.8: %.txt bin/txt2man
	@echo "    [txt2man] $<"
	@mkdir -p $(dir $@)
	$(V) bin/txt2man -t $(basename $(notdir $<)) \
	-s 8 -v "System Manager's Manual" -r "Device Mapper Tools" $< > $@

.PHONY: clean

clean:
	cargo clean
	$(RM) man8/*.8

HAS_PDATA_TOOLS_BINARY:=$(shell if [ -f $(PDATA_TOOLS) ]; then echo 1; fi)
ifneq ($(HAS_PDATA_TOOLS_BINARY),)

HAS_THIN_MIGRATE:=$(shell grep -qF thin_migrate.rs $(PDATA_TOOLS).d && echo 1)

ifneq ($(DISABLE_THIN_MIGRATE),)
$(warning DISABLE_THIN_MIGRATE variable is ignored, the pdata_tools binary exists and it has $(if $(HAS_THIN_MIGRATE),,no )thin_migrate tool built in)
endif

else

HAS_THIN_MIGRATE:=$(if $(DISABLE_THIN_MIGRATE),,1)

endif

TOOLS:=\
	cache_check \
	cache_dump \
	cache_metadata_size \
	cache_repair \
	cache_restore \
	cache_writeback \
	thin_check \
	thin_delta \
	thin_dump \
	thin_ls \
	$(if $(HAS_THIN_MIGRATE),thin_migrate) \
	thin_repair \
	thin_restore \
	thin_rmap \
	thin_metadata_size \
	thin_metadata_pack \
	thin_metadata_unpack \
	thin_trim \
	era_check \
	era_dump \
	era_invalidate \
	era_restore

# This must be two empty lines to get a newline.
define NEWLINE


endef

MANPAGES:=$(patsubst %,man8/%.8,$(TOOLS))

install: $(PDATA_TOOLS) $(MANPAGES)
	$(INSTALL_DIR) $(BINDIR)
	$(INSTALL_PROGRAM) $(PDATA_TOOLS) $(BINDIR)
	$(STRIP) $(BINDIR)/pdata_tools
	$(foreach tool, $(TOOLS), ln -s -f pdata_tools $(BINDIR)/$(tool); $(NEWLINE))
	$(INSTALL_DIR) $(MANPATH)/man8
	$(foreach tool, $(TOOLS), $(INSTALL_DATA) man8/$(tool).8 $(MANPATH)/man8; $(NEWLINE))

.PHONY: install

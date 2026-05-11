CC      ?= cc
CFLAGS  ?= -O2 -Wall -Wextra -std=c99 -Wno-unused-parameter -Wno-missing-field-initializers
LFSDIR  := ../gen_vectors/littlefs

SRCS := main.c $(LFSDIR)/lfs.c $(LFSDIR)/lfs_util.c
OUT  := build/verify_image

.PHONY: all clean

all: $(OUT)

$(OUT): $(SRCS) $(LFSDIR)/lfs.h $(LFSDIR)/lfs_util.h
	@mkdir -p build
	$(CC) $(CFLAGS) -I. $(SRCS) -o $@

clean:
	rm -rf build

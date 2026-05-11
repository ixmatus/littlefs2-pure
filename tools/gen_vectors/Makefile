CC      ?= cc
CFLAGS  ?= -O2 -Wall -Wextra -std=c99 -Wno-unused-parameter -Wno-missing-field-initializers
LFSDIR  := littlefs

SRCS := main.c $(LFSDIR)/lfs.c $(LFSDIR)/lfs_util.c
OUT  := build/gen_vectors

VECTOR_DIR := ../../tests/vectors

.PHONY: all vectors clean

all: $(OUT)

$(OUT): $(SRCS) $(LFSDIR)/lfs.h $(LFSDIR)/lfs_util.h
	@mkdir -p build
	$(CC) $(CFLAGS) -I. $(SRCS) -o $@

vectors: $(OUT)
	@mkdir -p $(VECTOR_DIR)
	$(OUT) $(VECTOR_DIR)

clean:
	rm -rf build

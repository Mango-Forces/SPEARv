#!/bin/sh
# Build the private, static, network-free CFITSIO 4.7.0 that spearv-fits links.
#
# Usage: fits/build-cfitsio.sh PREFIX
#
# build.rs reads the result from SPEARV_CFITSIO_PREFIX (default
# .build/native/cfitsio-4.7.0-prefix at the repository root) and refuses
# anything else: the header hashes must match commit
# d6d27653740864a0483e74cff23c53e34e1dceee, the archive must be static and
# reentrant, and it must not contain CFITSIO's HTTP/FTP/root/curl drivers.
# There is no system or bundled fallback.
#
# --disable-curl alone still leaves CFITSIO's built-in Unix HTTP/FTP parser.
# The two ac_cv_search cache flags also turn off HAVE_NET_SERVICES without
# patching upstream source.
set -eu

prefix=${1:?usage: build-cfitsio.sh PREFIX}
commit=d6d27653740864a0483e74cff23c53e34e1dceee
sha256=b99c555309eee9187615a9d64cb9a7ad84c2f3e94a0241c11bb024ccd5c56dde
url=https://codeload.github.com/HEASARC/cfitsio/tar.gz/$commit

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

curl -fsSL "$url" -o "$work/cfitsio.tar.gz"
echo "$sha256  $work/cfitsio.tar.gz" | sha256sum -c -
tar -xzf "$work/cfitsio.tar.gz" -C "$work"

cd "$work/cfitsio-$commit"
./configure --enable-reentrant --disable-curl --disable-shared \
  --prefix="$prefix" CFLAGS='-O2 -g0' \
  ac_cv_search_gethostbyname=no ac_cv_search_connect=no
make -j2 libcfitsio.la
make install

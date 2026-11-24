#!/bin/zsh
# Builds CodeWeavers' CrossOver Wine (GPL source, published by CodeWeavers)
# for x86_64 under Rosetta and installs it into target/wine-cx. D3DMetal is
# made for this Wine, and its memory APIs are new enough for VIctory.
#
# DEPS names an x86_64 prefix with FreeType and GnuTLS, such as the
# build/deps folder of the Minecraft Bedrock macOS setup.
set -eu
VERSION="${CROSSOVER_VERSION:-26.3.0}"
ROOT="${0:A:h:h:h}"
DEPS_REAL="${DEPS:?set DEPS to an x86_64 prefix with FreeType and GnuTLS}"
# Wine's build splits flags at spaces, and both the project and the deps
# can live under folders with spaces in their names. Space-free links to
# them keep every path in the build free of spaces.
ALIAS="$HOME/Library/Caches/victory-macos"
mkdir -p "$ALIAS" "$ROOT/target"
ln -sfn "$ROOT/target" "$ALIAS/target"
ln -sfn "$DEPS_REAL" "$ALIAS/deps"
DEPS="$ALIAS/deps"
WORK="$ALIAS/target/crossover"
OBJ="$WORK/build"
PREFIX="$ALIAS/target/wine-cx"
BREW="${BREW:-/opt/homebrew}"
x86() { arch -x86_64 /usr/bin/env PATH="$BREW/opt/bison/bin:$BREW/opt/flex/bin:$BREW/bin:/usr/bin:/bin:/usr/sbin:/sbin" "$@"; }

mkdir -p "$WORK" "$OBJ"
TAR="$WORK/crossover-sources-$VERSION.tar.gz"
if [[ ! -d $WORK/sources/wine ]]; then
  [[ -f $TAR ]] || curl -fL -o "$TAR" "https://media.codeweavers.com/pub/crossover/source/crossover-sources-$VERSION.tar.gz"
  tar -xzf "$TAR" -C "$WORK"
fi
SRC="$WORK/sources/wine"

cd "$OBJ"
if [[ ! -f Makefile ]]; then
  echo "configuring CrossOver Wine"
  # The prefix has spaces in it, so autoconf's default site file path would
  # be split at the first one. No site file is needed.
  CONFIG_SITE=/dev/null x86 "$SRC/configure" --prefix="$PREFIX" --enable-archs=x86_64 \
    CC="clang -arch x86_64" CXX="clang++ -arch x86_64" \
    CFLAGS="-g -O2 -I$DEPS/include -Wno-error=incompatible-function-pointer-types -Wno-error=int-conversion" \
    LDFLAGS="-L$DEPS/lib -Wl,-rpath,@loader_path/../.." \
    PKG_CONFIG_PATH="$DEPS/lib/pkgconfig" \
    FREETYPE_CFLAGS="-I$DEPS/include/freetype2" FREETYPE_LIBS="-L$DEPS/lib -lfreetype" \
    GNUTLS_CFLAGS="-I$DEPS/include" GNUTLS_LIBS="-L$DEPS/lib -lgnutls" \
    --without-gstreamer --without-sdl --without-cups --without-sane --without-krb5 \
    --without-netapi --without-pcap --without-usb --without-v4l2 --without-capi \
    --without-gphoto --without-x --without-wayland --without-pulse --without-alsa \
    --without-oss --without-udev --without-inotify --without-vulkan --without-opencl \
    --without-ffmpeg > configure.log 2>&1 || { tail -20 configure.log; exit 1; }
fi
echo "building CrossOver Wine"
x86 make -j"$(sysctl -n hw.ncpu)" > make.log 2>&1 || { grep -m5 -B2 'error' make.log; exit 1; }
x86 make install > install.log 2>&1
cp -f "$DEPS"/lib/lib{gmp.10,nettle.8,hogweed.6,gnutls.30,freetype.6}.dylib "$PREFIX/lib/"
echo "installed in $PREFIX"

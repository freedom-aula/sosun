#!/bin/zsh
# Builds x86_64 copies of GMP, Nettle, GnuTLS and FreeType into the prefix
# given as the first argument. Wine loads GnuTLS for TLS and FreeType for
# fonts. Homebrew on Apple Silicon only ships arm64 builds, which an x86_64
# Wine cannot load. The prefix path must have no spaces in it.
set -eu
PREFIX="$1"
SRC="${PREFIX:h}/deps-src"
WORK="${PREFIX:h}/deps-build"
BREW="${HOMEBREW_PREFIX:-/opt/homebrew}"
JOBS="$(sysctl -n hw.ncpu)"
x86() { arch -x86_64 /usr/bin/env PATH="$BREW/opt/bison/bin:$BREW/opt/flex/bin:$BREW/bin:/usr/bin:/bin:/usr/sbin:/sbin" "$@"; }

if [[ -f $PREFIX/lib/libgnutls.30.dylib && -f $PREFIX/lib/libfreetype.6.dylib ]]; then
  echo "libraries already built"
  exit 0
fi

# Each entry is a URL and the SHA-256 of its tarball.
DEPS=(
  "https://ftp.gnu.org/gnu/gmp/gmp-6.3.0.tar.xz a3c2b80201b89e68616f4ad30bc66aee4927c3ce50e33929ca819d5c43538898"
  "https://ftp.gnu.org/gnu/nettle/nettle-3.10.tar.gz b4c518adb174e484cb4acea54118f02380c7133771e7e9beb98a0787194ee47c"
  "https://www.gnupg.org/ftp/gcrypt/gnutls/v3.8/gnutls-3.8.13.tar.xz ffed8ec1bf09c2426d4f14aae377de4753b53e537d685e604e99a8b16ca9c97e"
  "https://download.savannah.gnu.org/releases/freetype/freetype-2.14.3.tar.xz 36bc4f1cc413335368ee656c42afca65c5a3987e8768cc28cf11ba775e785a5f"
)
mkdir -p "$SRC" "$WORK"
for entry in $DEPS; do
  url="${entry%% *}"; sum="${entry##* }"; file="$SRC/${url:t}"
  [[ -f $file ]] || { echo "downloading ${url:t}"; curl -fL --retry 3 -o "$file.part" "$url" && mv "$file.part" "$file"; }
  echo "$sum  $file" | shasum -a 256 -c --quiet || { echo "checksum mismatch for ${url:t}" >&2; exit 1; }
  tar -xf "$file" -C "$WORK"
done

common=(--host=x86_64-apple-darwin --build=aarch64-apple-darwin --prefix="$PREFIX")
export CC="clang -arch x86_64" CXX="clang++ -arch x86_64"
export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig" CPPFLAGS="-I$PREFIX/include" LDFLAGS="-L$PREFIX/lib"
build() {
  local dir="$WORK/$1"; shift
  echo "building ${dir:t}"
  ( cd "$dir" && CONFIG_SITE=/dev/null x86 ./configure "${common[@]}" "$@" > configure.log 2>&1 \
      && x86 make -j"$JOBS" > make.log 2>&1 && x86 make install > install.log 2>&1 ) \
    || { echo "building ${dir:t} failed, see the logs in $dir" >&2; exit 1; }
}
build gmp-6.3.0 --disable-assembly --enable-shared --enable-static
build nettle-3.10 --disable-documentation --disable-assembler --enable-shared --enable-static
build gnutls-3.8.13 --with-included-libtasn1 --with-included-unistring --without-p11-kit \
  --disable-doc --disable-tests --disable-tools --without-idn --without-brotli --without-zstd \
  --disable-guile --disable-cxx --disable-nls --disable-libdane --disable-openssl-compatibility \
  --disable-hardware-acceleration --enable-shared --enable-static=no
build freetype-2.14.3 --without-harfbuzz --without-png --without-bzip2 --without-brotli
echo "libraries installed in $PREFIX"

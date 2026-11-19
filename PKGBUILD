# Maintainer: Kotokvit <213886740+Kotokvit@users.noreply.github.com>
# Contributor: POLER Engineering Core

pkgname=poler-harvest
pkgver=0.96.0
pkgrel=1
pkgdesc="Sovereign Ultra-Fast Filesystem Harvester & L1-L3 Address Crystal Scanner (successor to find + ripgrep)"
arch=('x86_64')
url="https://github.com/poler-engine-org/poler-harvest"
license=('MIT' 'Apache-2.0')
depends=('gcc-libs' 'glibc' 'zstd')
makedepends=('cargo' 'pkgconf')
source=("$pkgname-$pkgver.tar.gz::$url/archive/refs/tags/v$pkgver.tar.gz")
sha256sums=('SKIP')

prepare() {
    cd "$pkgname-$pkgver"
    export RUSTUP_TOOLCHAIN=stable
    cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
    if [ -d "$pkgname-$pkgver" ]; then
        cd "$pkgname-$pkgver"
    fi
    export RUSTUP_TOOLCHAIN=stable
    cargo build --release --locked
}

check() {
    if [ -d "$pkgname-$pkgver" ]; then
        cd "$pkgname-$pkgver"
    fi
    export RUSTUP_TOOLCHAIN=stable
    cargo test --release --locked
}

package() {
    cd "$startdir"
    install -Dm755 "target/release/$pkgname" "$pkgdir/usr/bin/$pkgname"
    install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
}

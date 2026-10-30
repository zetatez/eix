# Maintainer: eix contributors
pkgname=eix
pkgver=$(sed -n 's/^version = "\(.*\)"/\1/p' "$startdir/Cargo.toml" | head -n 1)
pkgrel=1
pkgdesc='Coding agent with a multi-session TUI and editor plugin support'
arch=('x86_64' 'aarch64')
url='https://github.com/zetatez/eix'
license=('custom')
# makepkg's -flto=auto breaks the bundled SQLite static link.
options=('!lto' '!debug')
depends=('gcc-libs' 'glibc')
makedepends=('cargo' 'rust')
optdepends=(
  'git: worktree support'
)
source=()
sha256sums=()

build() {
  cd "$startdir"
  CARGO_TARGET_DIR="$startdir/target" cargo build --workspace --release --locked
}

package() {
  cd "$startdir"
  install -Dm755 "$startdir/target/release/eix" "$pkgdir/usr/bin/eix"
  install -Dm755 "$startdir/target/release/eixd" "$pkgdir/usr/bin/eixd"
  install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
  install -Dm644 DESIGN.md "$pkgdir/usr/share/doc/$pkgname/DESIGN.md"
  install -Dm644 docs/nvim.md "$pkgdir/usr/share/doc/$pkgname/docs/nvim.md"
}

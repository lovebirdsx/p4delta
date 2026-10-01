#!/usr/bin/env bash
#
# 把与当前主机匹配的 p4 与 p4d 下载到 vendor/，供 e2e 沙箱使用。
#
# 本地开发者若已装了 P4V（自带 p4d），不必跑这个脚本——探测顺序里
# 系统安装也有一席之地。CI 上则必须跑：runner 上什么都没有。
#
# 可重复运行：文件已存在且校验通过就跳过下载。
set -euo pipefail

# 升级时只改这一行。目录名与文件名都跟着它走。
VERSION="${P4_VERSION:-r24.1}"
BASE="https://cdist2.perforce.com/perforce/${VERSION}"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dest="$root/vendor"

# ---- 平台 → cdist2 目录名 ----

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux) platform=linux ;;
  Darwin) platform=macos ;;
  # Git Bash / MSYS2 / Cygwin 都归到 Windows。
  MINGW* | MSYS* | CYGWIN*) platform=windows ;;
  *)
    echo "unsupported OS: $os" >&2
    exit 1
    ;;
esac

case "$platform/$arch" in
  linux/x86_64) dist=bin.linux26x86_64 ;;
  linux/aarch64) dist=bin.linux26aarch64 ;;
  macos/arm64) dist=bin.macosx12arm64 ;;
  # macosx12 的 x86_64 目录里只有 p4api，命令行二进制仍留在 macosx1015 下。
  macos/x86_64) dist=bin.macosx1015x86_64 ;;
  windows/x86_64) dist=bin.ntx64 ;;
  *)
    echo "unsupported platform: $platform/$arch" >&2
    exit 1
    ;;
esac

# 只有 Windows 的二进制带后缀。
if [ "$platform" = windows ]; then
  exe=.exe
else
  exe=
fi

# ---- 校验工具自适应 ----

# 优先 GNU 的 sha256sum，macOS 上回退到 perl 的 shasum。
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# SHA256SUMS 是 BSD 风格，文件名那列带一个 `*` 前缀：`<hex> *<name>`。
checksum_for() {
  awk -v name="$1" '{ sub(/^\*/, "", $2); if ($2 == name) { print $1; exit } }' "$sums"
}

# ---- 下载 ----

fetch() {
  local name="$1" target="$dest/$1" want got
  want="$(checksum_for "$name")"
  if [ -z "$want" ]; then
    echo "no checksum for $name in $dist/SHA256SUMS" >&2
    exit 1
  fi

  if [ -f "$target" ] && [ "$(sha256_of "$target")" = "$want" ]; then
    echo "vendor/$name: already present and verified"
    return
  fi

  echo "vendor/$name: downloading from $BASE/$dist/$name"
  curl -fsSL "$BASE/$dist/$name" -o "$target.part"

  # 校验失败必须硬失败：宁可 e2e 全部跳过，也不能跑在一个来路不明的服务端上。
  got="$(sha256_of "$target.part")"
  if [ "$got" != "$want" ]; then
    rm -f "$target.part"
    echo "checksum mismatch for $name: expected $want, got $got" >&2
    exit 1
  fi

  mv "$target.part" "$target"
  chmod +x "$target"

  # macOS 会给下载回来的可执行文件打隔离标记，Gatekeeper 会直接拦下。
  if [ "$platform" = macos ]; then
    xattr -d com.apple.quarantine "$target" 2>/dev/null || true
  fi
}

mkdir -p "$dest"

sums="$(mktemp)"
trap 'rm -f "$sums"' EXIT
curl -fsSL "$BASE/$dist/SHA256SUMS" -o "$sums"

fetch "p4$exe"
fetch "p4d$exe"

# ---- 冒烟 ----

# 能打印版本，才说明这个二进制在这台机器上跑得起来（架构/依赖都对）。
# cwd 换到 vendor 再跑：p4d 会读当前目录下名为 license 的文件，
# 仓库根正好有个 LICENSE，会让它报 "Error reading license file." 后退出。
smoke() {
  if ! (cd "$dest" && "./$1" -V >/dev/null 2>&1); then
    echo "vendor/$1 does not run on this machine" >&2
    exit 1
  fi
}

smoke "p4d$exe"
smoke "p4$exe"

echo "p4 tools for $dist are ready in $dest"

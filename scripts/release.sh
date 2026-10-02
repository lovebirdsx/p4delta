#!/usr/bin/env bash
#
# 发布一个版本：版本号写进 Cargo.toml / p4delta.exe.manifest / Cargo.lock，
# 本地跑一遍门禁，然后提交、打附注 tag、推送。
#
# 推送之后由 .github/workflows/release.yml 接手：复用 CI 当门禁、构建、断言产物、
# 打包 zip 与 SHA256SUMS、建 release。这个脚本只负责到「把 tag 推上去」为止。
set -euo pipefail

usage() {
  cat <<'EOF'
用法：bash scripts/release.sh [<X.Y.Z>] [选项]

不写版本号就自动升：默认补丁号 +1（0.1.3 → 0.1.4）。
  --patch         补丁号 +1（默认）
  --minor         次版本号 +1，补丁号归零（0.1.3 → 0.2.0）
  --major         主版本号 +1，后面归零（0.1.3 → 1.0.0）

版本号有三个去处，脚本一次改完：Cargo.toml 的 version、p4delta.exe.manifest 里
assemblyIdentity 的 version（四段的 X.Y.Z.0）、Cargo.lock 里 p4delta 的条目。
（exe 的 VERSIONINFO 不用管，build.rs 从 Cargo.toml 现算。）

改完先跑一遍本地门禁（fmt / clippy / test），过了才提交、打 tag、推送 ——
tag 一旦推出去 release.yml 就会跑，门禁在本地红比在 CI 红便宜得多。

其他选项：
  --dry-run       只做检查并打印将要做什么，不改文件、不提交、不打 tag、不推送
  --skip-check    跳过本地门禁（CI 里还会再跑一遍，但那要等 tag 推出去之后）
  --no-push       本地做完：改文件、跑门禁、提交、打 tag，但不推送
  --yes           推送前不再确认
  --branch <名>   要求当前分支是 <名>（默认 main）
  --remote <名>   远端名（默认 origin）
  -h, --help      显示这段

例子：
  bash scripts/release.sh --dry-run       # 先看一眼（版本号自动升）
  bash scripts/release.sh                 # 发 0.1.3 → 0.1.4
  bash scripts/release.sh --minor         # 发 0.1.3 → 0.2.0
  bash scripts/release.sh 0.1.4           # 指定版本号，不自动升
EOF
}

die() { printf 'error: %s\n' "$*" >&2; exit 1; }
step() { printf '==> %s\n' "$*"; }

# 就地替换，不用 sed -i：BSD sed 与 GNU sed 的写法不兼容，而这个脚本三种平台都会跑。
# 走 cat 而不是 mv，为的是保住原文件的权限位与 inode。
edit() {
  local file="$1" script="$2" tmp
  tmp="$(mktemp)"
  sed "$script" "$file" >"$tmp"
  cat "$tmp" >"$file"
  rm -f "$tmp"
}

# 三段数字逐段比大小；$1 比 $2 大时为真。
version_gt() {
  local -a a b
  local i
  IFS=. read -r -a a <<<"$1"
  IFS=. read -r -a b <<<"$2"
  for i in 0 1 2; do
    if [ "${a[i]:-0}" -gt "${b[i]:-0}" ]; then return 0; fi
    if [ "${a[i]:-0}" -lt "${b[i]:-0}" ]; then return 1; fi
  done
  return 1
}

# 升一段版本号。加法写成 10#：$((08 + 1)) 会被当成八进制而报错。
bump_version() {
  local -a p
  IFS=. read -r -a p <<<"$1"
  case "$2" in
    major) printf '%d.0.0\n' "$((10#${p[0]} + 1))" ;;
    minor) printf '%d.%d.0\n' "$((10#${p[0]}))" "$((10#${p[1]} + 1))" ;;
    patch) printf '%d.%d.%d\n' "$((10#${p[0]}))" "$((10#${p[1]}))" "$((10#${p[2]} + 1))" ;;
    *) die "不认识的升级幅度：$2" ;;
  esac
}

# 把推送命令拼出来，成功与失败的提示里都要用。
push_hint() {
  printf 'git push %s %s && git push %s %s' "$REMOTE" "$BRANCH" "$REMOTE" "$TAG"
}

# ---- 参数 ----

VERSION=
BUMP=patch
BUMP_SET=0
AUTO_BUMP=0
DRY_RUN=0
SKIP_CHECK=0
NO_PUSH=0
ASSUME_YES=0
BRANCH=main
REMOTE=origin

while [ $# -gt 0 ]; do
  case "$1" in
    --patch) BUMP=patch; BUMP_SET=1 ;;
    --minor) BUMP=minor; BUMP_SET=1 ;;
    --major) BUMP=major; BUMP_SET=1 ;;
    --dry-run) DRY_RUN=1 ;;
    --skip-check) SKIP_CHECK=1 ;;
    --no-push) NO_PUSH=1 ;;
    --yes | -y) ASSUME_YES=1 ;;
    --branch) BRANCH="${2:?--branch 需要跟一个分支名}"; shift ;;
    --remote) REMOTE="${2:?--remote 需要跟一个远端名}"; shift ;;
    -h | --help) usage; exit 0 ;;
    -*) usage >&2; die "未知选项：$1" ;;
    *)
      [ -z "$VERSION" ] || die "多给了一个版本号：$1"
      VERSION="$1"
      ;;
  esac
  shift
done

cd "$(git rev-parse --show-toplevel 2>/dev/null)" || die '不在 git 仓库里'

# 显式给的版本号先验格式；没给的话，等读出当前版本再算。
if [ -n "$VERSION" ]; then
  [ "$BUMP_SET" -eq 0 ] ||
    die "已经给了版本号 $VERSION，就不要再给 --patch / --minor / --major 了"
  printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' ||
    die "版本号要写成 X.Y.Z 三段数字，不带 v 前缀（v 由脚本加）：$VERSION"
fi

# ---- 前置检查：宁可在这里红，也别把 tag 推出去再发现 ----

branch="$(git rev-parse --abbrev-ref HEAD)"
[ "$branch" = "$BRANCH" ] || die "当前在 $branch 分支上；发布要在 $BRANCH 上做（换分支用 --branch）"

dirty="$(git status --porcelain)"
if [ -n "$dirty" ]; then
  printf '%s\n' "$dirty" >&2
  die '工作区不干净：发布的提交里只该有版本号这一处改动，先提交或收起来'
fi

# 只认行首那一处 version = ，多了就说明 Cargo.toml 的结构变了，脚本不该瞎猜。
found="$(grep -c '^version = ' Cargo.toml || true)"
[ "$found" -eq 1 ] || die "Cargo.toml 里行首的 version = 有 $found 处，脚本只认识一处，手工改吧"
current="$(sed -n 's/^version = "\([^"]*\)"$/\1/p' Cargo.toml)"
[ -n "$current" ] || die '读不出 Cargo.toml 的 version'

# release.yml 的 tag 断言、这里的自动升号与「必须变大」检查都只懂三段数字，口径一致。
printf '%s' "$current" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' ||
  die "Cargo.toml 当前的 version 是 $current，不是 X.Y.Z 三段数字，脚本升不了号也判不了大小，先把它理顺"

if [ -z "$VERSION" ]; then
  VERSION="$(bump_version "$current" "$BUMP")"
  AUTO_BUMP=1
fi

TAG="v$VERSION"

if [ "$AUTO_BUMP" -eq 1 ]; then
  how="（没给版本号，自动升 $BUMP）"
else
  how=
fi

# 上一版漏同步 manifest 的话，这里先拦住：脚本会把它改成新版本号，等于把错误悄悄
# 抹平，而 manifest 的 version 本来该由测试里的用例盯着。
grep -qF "version=\"$current.0\"" p4delta.exe.manifest ||
  die "p4delta.exe.manifest 的版本与 Cargo.toml 的 $current 对不上，先把上一版漏掉的同步补上"

version_gt "$VERSION" "$current" ||
  die "新版本 $VERSION 不比当前的 $current 大。同一版本不能发两次，要重发得先删掉远端 tag"

if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  die "本地已经有 tag $TAG"
fi

git fetch --quiet "$REMOTE" "$BRANCH" || die "取不到 $REMOTE/$BRANCH，检查网络与远端名（--remote）"
if git ls-remote --exit-code --tags "$REMOTE" "refs/tags/$TAG" >/dev/null 2>&1; then
  die "远端已经有 tag $TAG。同一版本不能发两次：换一个版本号；确实要重发就先删掉远端 tag"
fi

behind="$(git rev-list --count "HEAD..FETCH_HEAD")"
[ "$behind" -eq 0 ] ||
  die "本地落后 $REMOTE/$BRANCH $behind 个提交，先 git pull —— 否则 tag 会打在旧提交上"

# ---- 到这里为止都没动过任何东西 ----

if [ "$DRY_RUN" -eq 1 ]; then
  step "dry run：检查都过了。真跑的话会做这些："
  printf '    版本号：%s → %s%s\n' "$current" "$VERSION" "$how"
  printf '    1. 写进 Cargo.toml、p4delta.exe.manifest、Cargo.lock 三处\n'
  if [ "$SKIP_CHECK" -eq 1 ]; then
    printf '    2. 跳过本地门禁（--skip-check）\n'
  else
    printf '    2. 跑本地门禁：fmt / clippy / test\n'
  fi
  printf '    3. 提交，打附注 tag %s\n' "$TAG"
  if [ "$NO_PUSH" -eq 1 ]; then
    printf '    4. 不推送（--no-push）\n'
  else
    printf '    4. 推 %s 与 %s 到 %s\n' "$BRANCH" "$TAG" "$REMOTE"
  fi
  exit 0
fi

step "改版本号：$current → $VERSION$how"

edit Cargo.toml "s/^version = \"[^\"]*\"\$/version = \"$VERSION\"/"
found="$(grep -c "^version = \"$VERSION\"\$" Cargo.toml || true)"
[ "$found" -eq 1 ] || die 'Cargo.toml 的 version 没改对，改动已留在工作区，先看再 git checkout'

# 行首锚定，为的是别碰同一行的 <assembly manifestVersion="1.0">；末尾不锚定，
# 因为这一行后面还跟着 "/>"。
edit p4delta.exe.manifest "s/^\( *\)version=\"[^\"]*\"/\1version=\"$VERSION.0\"/"
found="$(grep -c "^ *version=\"$VERSION\.0\"" p4delta.exe.manifest || true)"
[ "$found" -eq 1 ] || die 'p4delta.exe.manifest 的 version 没改对，改动已留在工作区，先看再 git checkout'
grep -qF 'manifestVersion="1.0"' p4delta.exe.manifest ||
  die 'p4delta.exe.manifest 的 manifestVersion 被改到了，改动已留在工作区，先看再 git checkout'

# Cargo.lock 里也有 p4delta 的版本号，不更新的话 CI 的 --locked 会直接红。
# --offline：只更新工作区成员，不需要网络。
cargo update --workspace --offline
found="$(grep -A1 '^name = "p4delta"$' Cargo.lock | grep -c "^version = \"$VERSION\"\$" || true)"
[ "$found" -eq 1 ] || die 'Cargo.lock 里 p4delta 的版本没跟着更新'

if [ "$SKIP_CHECK" -eq 1 ]; then
  step '跳过本地门禁（--skip-check）'
else
  step '本地门禁'
  cargo fmt --all -- --check
  cargo clippy --all-targets --all-features --locked -- -D warnings
  cargo nextest run --all-targets --all-features --locked
  printf '    （机器上没有 p4/p4d 时 e2e 会自己跳过；CI 上不跳，那边设了 P4_E2E_REQUIRED）\n'
fi

step "提交并打 tag $TAG"
git add Cargo.toml Cargo.lock p4delta.exe.manifest
git commit -m "chore: 发布 $TAG"
git tag -a "$TAG" -m "$TAG"

if [ "$NO_PUSH" -eq 1 ]; then
  step '没推送（--no-push）'
  printf '    提交和 tag 都在本地了。要推的时候：%s\n' "$(push_hint)"
  exit 0
fi

if [ "$ASSUME_YES" -eq 0 ]; then
  printf '把提交与 tag %s 推到 %s？[y/N] ' "$TAG" "$REMOTE"
  # read 在管道里读到 EOF 会返回非 0，set -e 会当场静默退出——这里接住它，走「没推送」。
  read -r reply || reply=
  case "$reply" in
    [yY]*) ;;
    *)
      step '没有推送'
      printf '    提交和 tag 都在本地了。要推的时候：%s\n' "$(push_hint)"
      exit 0
      ;;
  esac
fi

step "推送到 $REMOTE"
git push "$REMOTE" "$BRANCH" || die "推 $BRANCH 失败。提交与 tag 都在本地，处理完这样重推：$(push_hint)"
git push "$REMOTE" "$TAG" || die "推 tag 失败。分支已经推上去了，修好之后：git push $REMOTE $TAG"

step '推完了'
slug=
url="$(git remote get-url "$REMOTE" 2>/dev/null || true)"
case "$url" in
  git@github.com:*) slug="${url#git@github.com:}" ;;
  ssh://git@github.com/*) slug="${url#ssh://git@github.com/}" ;;
  https://github.com/*) slug="${url#https://github.com/}" ;;
esac
slug="${slug%.git}"
if [ -n "$slug" ]; then
  printf '    release.yml 开始跑了：https://github.com/%s/actions/workflows/release.yml\n' "$slug"
  printf '    盯进度：gh run watch\n'
fi
printf '    release 建好后，在装了 P4V 的机器上按 CONTRIBUTING.md「发布检查清单」的最后一步核对一遍\n'

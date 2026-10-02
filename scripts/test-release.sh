#!/usr/bin/env bash
#
# scripts/release.sh 的黑盒测试。
#
# 在一个一次性 git 仓库里真跑 release.sh：origin 是一个本地裸仓库（推送那一步是真推），
# cargo 换成 PATH 最前面的垫片（不编译、不联网，只模拟 `cargo update --workspace` 对
# 唯一工作区成员做的那一件事）。被测的三个文件用的是仓库里真实的 Cargo.toml /
# p4delta.exe.manifest / Cargo.lock，格式不会跑偏。
#
# 不碰真实仓库：一切都在 mktemp 目录里，退出即清。
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
script="$root/scripts/release.sh"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

bin="$tmp/bin"
origin="$tmp/origin.git"
repo="$tmp/repo"
other="$tmp/other"
log="$tmp/cargo.log"
out="$tmp/out.txt"
err="$tmp/err.txt"
mkdir -p "$bin"
export CARGO_SHIM_LOG="$log"

# cargo 垫片。真实 cargo 的行为不在这里验证（那是 cargo 自己的事），这里只需要它把
# Cargo.lock 里工作区成员的版本改掉，好让整条链路能走完；顺便把参数记下来，供用例
# 断言门禁那三条命令确实被调用了。
cat >"$bin/cargo" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$CARGO_SHIM_LOG"
if [ "${1:-}" = update ]; then
  v="$(sed -n 's/^version = "\([^"]*\)"$/\1/p' Cargo.toml)"
  tmp="$(mktemp)"
  awk -v v="$v" '
    /^name = "p4delta"$/ { print; if (getline > 0) print "version = \"" v "\""; next }
    { print }
  ' Cargo.lock >"$tmp"
  cat "$tmp" >Cargo.lock
  rm -f "$tmp"
fi
SHIM
chmod +x "$bin/cargo"

setup() {
  rm -rf "$repo" "$origin" "$other"
  # 裸仓库也要 -b main：不带的话它的 HEAD 指向 master，clone 出来会是个空工作区。
  git init --quiet --bare -b main "$origin"
  git init --quiet -b main "$repo"
  git -C "$repo" config user.name 'p4delta 测试'
  git -C "$repo" config user.email 'test@example.com'
  # 别让开发机的 gpgsign 配置把提交或 tag 卡住。
  git -C "$repo" config commit.gpgsign false
  git -C "$repo" config tag.gpgsign false

  mkdir -p "$repo/scripts"
  cp "$script" "$repo/scripts/release.sh"
  cp "$root/Cargo.toml" "$root/Cargo.lock" "$root/p4delta.exe.manifest" "$repo/"
  git -C "$repo" add -A
  git -C "$repo" commit --quiet -m '初始提交'
  git -C "$repo" remote add origin "$origin"
  git -C "$repo" push --quiet -u origin main

  : >"$log"
  : >"$out"
  : >"$err"
}

# 跑一次 release.sh，返回它的退出码；输出分别落在 $out 与 $err。
run() {
  local rc=0
  (cd "$repo" && PATH="$bin:$PATH" bash scripts/release.sh "$@") >"$out" 2>"$err" || rc=$?
  return "$rc"
}

fail() { printf '断言失败：%s\n' "$*" >&2; exit 1; }

assert_eq() { [ "$1" = "$2" ] || fail "$3：期望 [$1]，实际 [$2]"; }
assert_contains() { grep -qF -- "$1" "$2" || fail "$3：$2 里没有 [$1]"; }
assert_clean() { [ -z "$(git -C "$repo" status --porcelain)" ] || fail "$1：工作区应当是干净的"; }

repo_version() { sed -n 's/^version = "\([^"]*\)"$/\1/p' "$repo/Cargo.toml"; }

# 期望的升级结果，用 awk 独立算一遍——不要拿脚本自己的算法去断言脚本自己。
bump_of() {
  awk -F'[.]' -v part="$2" '{
    if (part == "major") print $1 + 1 ".0.0"
    else if (part == "minor") print $1 "." $2 + 1 ".0"
    else print $1 "." $2 "." $3 + 1
  }' <<<"$1"
}

# ---- 用例 ----

function test_dry_run_changes_nothing {
  setup
  local before
  before="$(repo_version)"

  run --dry-run 9.9.9 || fail "dry run 不该失败（退出码 $?）：$(cat "$err")"

  assert_eq "$before" "$(repo_version)" 'dry run 不该改版本号'
  assert_eq '' "$(git -C "$repo" tag -l)" 'dry run 不该打 tag'
  assert_eq '1' "$(git -C "$repo" rev-list --count HEAD)" 'dry run 不该产生新提交'
  assert_clean 'dry run 之后'
}

function test_releases {
  setup

  run 9.9.9 --yes || fail "发布失败（退出码 $?）：$(cat "$err")"

  # 三个文件各改一处
  assert_eq '9.9.9' "$(repo_version)" 'Cargo.toml 的版本号'
  assert_contains 'version="9.9.9.0"' "$repo/p4delta.exe.manifest" 'manifest 的程序集版本'
  assert_contains 'manifestVersion="1.0"' "$repo/p4delta.exe.manifest" 'manifest 的 manifestVersion 不该被动'
  assert_contains 'version = "9.9.9"' "$repo/Cargo.lock" 'Cargo.lock 里 p4delta 的版本'

  # 门禁三条命令都跑过，而且 Cargo.lock 是走 cargo 刷新的
  assert_contains 'fmt --all -- --check' "$log" '门禁应当跑 fmt'
  assert_contains 'clippy --all-targets' "$log" '门禁应当跑 clippy'
  assert_contains 'test --all-targets' "$log" '门禁应当跑 test'
  assert_contains 'update --workspace' "$log" '应当刷新 Cargo.lock'

  # 提交只含那三个文件
  assert_eq 'chore: 发布 v9.9.9' "$(git -C "$repo" log -1 --pretty=%s)" '提交标题'
  assert_eq 'Cargo.lock Cargo.toml p4delta.exe.manifest' \
    "$(git -C "$repo" show --pretty=format: --name-only HEAD | sort | tr '\n' ' ' | sed 's/ $//')" \
    '提交里的文件'

  # 附注 tag，指向刚提交的提交，并且推到了 origin
  assert_eq 'tag' "$(git -C "$repo" cat-file -t v9.9.9)" 'v9.9.9 应当是附注 tag'
  assert_eq "$(git -C "$repo" rev-parse HEAD)" "$(git -C "$repo" rev-parse 'v9.9.9^{commit}')" \
    'tag 应当指向刚提交的提交'
  assert_eq 'v9.9.9' "$(git -C "$origin" tag -l v9.9.9)" 'tag 应当推到远端'
  assert_eq "$(git -C "$repo" rev-parse HEAD)" "$(git -C "$origin" rev-parse main)" '分支应当推到远端'
  assert_clean '发布之后'
}

function test_auto_bumps_patch {
  setup
  local before after
  before="$(repo_version)"

  run --skip-check --yes || fail "不带版本号发布失败（退出码 $?）：$(cat "$err")"

  after="$(repo_version)"
  assert_eq "$(bump_of "$before" patch)" "$after" '不带版本号应当把补丁号 +1'
  assert_eq "v$after" "$(git -C "$origin" tag -l)" 'tag 用的应当是自动升出来的版本'
  assert_contains '自动升 patch' "$out" '应当说明版本号是自动升的'
}

function test_auto_bumps_minor {
  setup
  local before after
  before="$(repo_version)"

  run --minor --skip-check --yes || fail "--minor 发布失败（退出码 $?）：$(cat "$err")"

  after="$(repo_version)"
  assert_eq "$(bump_of "$before" minor)" "$after" '--minor 应当升次版本号并把补丁号归零'
}

function test_auto_bumps_major {
  setup
  local before after
  before="$(repo_version)"

  run --major --skip-check --yes || fail "--major 发布失败（退出码 $?）：$(cat "$err")"

  after="$(repo_version)"
  assert_eq "$(bump_of "$before" major)" "$after" '--major 应当升主版本号并把后面归零'
}

function test_auto_bump_respects_existing_tags {
  setup
  # 自动升出来的那个版本已经有 tag 了（比如上一次发布卡在推送之后）——
  # 自动升号不该把「同一版本不能发两次」这条绕过。
  local next
  next="$(bump_of "$(repo_version)" patch)"
  git -C "$repo" push --quiet origin HEAD:refs/tags/"v$next"

  if run --yes; then fail "自动升出来 v$next 已经有 tag 了，不该继续"; fi
  assert_contains "远端已经有 tag v$next" "$err" '错误信息'
}

function test_rejects_version_with_bump_flag {
  setup
  local before
  before="$(repo_version)"

  if run 9.9.9 --minor --yes; then fail '既给了版本号又给了 --minor 时不该继续'; fi
  assert_contains '就不要再给' "$err" '错误信息'
  assert_eq "$before" "$(repo_version)" '拒绝时不该改版本号'
}

function test_skip_check_skips_the_gate {
  setup

  run 9.9.9 --skip-check --yes || fail "发布失败（退出码 $?）：$(cat "$err")"

  if grep -qE '^(fmt|clippy|test) ' "$log"; then
    fail "--skip-check 不该跑门禁，实际调用：$(cat "$log")"
  fi
  assert_contains 'update --workspace' "$log" '--skip-check 不该连 Cargo.lock 的刷新一起跳过'
}

function test_rejects_dirty_tree {
  setup
  # 拿一个已跟踪的文件弄脏工作区（版本号那一行还在，所以挡住它的只可能是这条检查）
  printf '\n' >>"$repo/p4delta.exe.manifest"
  local before
  before="$(repo_version)"

  if run 9.9.9 --yes; then fail '工作区不干净时不该继续'; fi
  assert_contains '工作区不干净' "$err" '错误信息'
  assert_eq "$before" "$(repo_version)" '拒绝时不该改版本号'
  assert_eq '' "$(git -C "$repo" tag -l)" '拒绝时不该打 tag'
}

function test_rejects_existing_tag {
  setup
  git -C "$repo" tag -a v9.9.9 -m v9.9.9
  local before
  before="$(repo_version)"

  if run 9.9.9 --yes; then fail '本地已有同名 tag 时不该继续'; fi
  assert_contains '本地已经有 tag' "$err" '错误信息'
  assert_eq "$before" "$(repo_version)" '拒绝时不该改版本号'
}

function test_rejects_remote_tag {
  setup
  # 只推到远端，本地没有 —— 挡住它的必须是远端那一条检查
  git -C "$repo" push --quiet origin HEAD:refs/tags/v9.9.9

  if run 9.9.9 --yes; then fail '远端已有同名 tag 时不该继续'; fi
  assert_contains '远端已经有 tag' "$err" '错误信息'
}

function test_rejects_bad_version {
  setup

  if run 9.9 --yes; then fail '两段的版本号不该被接受'; fi
  assert_contains 'X.Y.Z' "$err" '错误信息'

  if run v9.9.9 --yes; then fail '带 v 前缀的版本号不该被接受（v 由脚本自己加）'; fi
  assert_eq '1' "$(git -C "$repo" rev-list --count HEAD)" '拒绝时不该产生提交'
}

function test_rejects_version_not_greater {
  setup
  local current
  current="$(repo_version)"

  if run "$current" --yes; then fail '重发同一版本不该被接受'; fi
  assert_contains '不比当前的' "$err" '错误信息'

  if run 0.0.1 --yes; then fail '更小的版本号不该被接受'; fi
  assert_eq "$current" "$(repo_version)" '拒绝时不该改版本号'
}

function test_rejects_wrong_branch {
  setup
  git -C "$repo" checkout --quiet -b feature

  if run 9.9.9 --yes; then fail '不在 main 上时不该继续'; fi
  assert_contains '分支' "$err" '错误信息'
  assert_eq '' "$(git -C "$repo" tag -l)" '拒绝时不该打 tag'
}

function test_rejects_diverged_remote {
  setup
  # 别人往 origin 推了一个提交，本地没跟上 —— tag 会打在旧提交上，必须先拦住
  git clone --quiet "$origin" "$other"
  git -C "$other" config user.name '别人'
  git -C "$other" config user.email 'other@example.com'
  printf 'x\n' >"$other/notes.txt"
  git -C "$other" add -A
  git -C "$other" commit --quiet -m '别人推的提交'
  git -C "$other" push --quiet origin main

  if run 9.9.9 --yes; then fail '本地落后远端时不该继续'; fi
  assert_contains '落后' "$err" '错误信息'
  assert_eq '1' "$(git -C "$repo" rev-list --count HEAD)" '拒绝时不该产生提交'
}

# ---- 跑 ----

cases=(
  test_dry_run_changes_nothing
  test_releases
  test_auto_bumps_patch
  test_auto_bumps_minor
  test_auto_bumps_major
  test_auto_bump_respects_existing_tags
  test_rejects_version_with_bump_flag
  test_skip_check_skips_the_gate
  test_rejects_dirty_tree
  test_rejects_existing_tag
  test_rejects_remote_tag
  test_rejects_bad_version
  test_rejects_version_not_greater
  test_rejects_wrong_branch
  test_rejects_diverged_remote
)

failed=0
printf 'release.sh 测试\n'

for case in "${cases[@]}"; do
  if ("$case") 2>"$tmp/case-err.txt"; then
    printf '  ok   %s\n' "$case"
  else
    failed=$((failed + 1))
    printf '  FAIL %s\n' "$case"
    sed 's/^/       /' "$tmp/case-err.txt"
  fi
done

if [ "$failed" -gt 0 ]; then
  printf '%s 个用例失败\n' "$failed"
  exit 1
fi
printf '全部通过\n'

#!/usr/bin/env bash
#
# 把**本地构建**的 p4delta 装到 ~/.local/bin，让 WSL / Linux 里直接敲 `p4delta` 就能用。
#
# 与 scripts/install-local.ps1 是同一件事的两个平台版本：那个把 exe 铺进 P4V 的安装目录、
# 再把工具定义写进 customtools.xml。P4V 是 Windows 程序、只能启动 Windows 版 exe，所以在
# Linux 上那两件事都没有对应物——这里只做「把本地构建放到 PATH 上」，P4V 一概不碰。
#
# 按 bash 3.2 也能跑的语法写（macOS 的 /bin/bash 是 3.2）：不用数组、不用 ${var,,}、
# 不用 mapfile。这条约束没有别处会提醒，改之前先看一眼。
#
# 用法：bash scripts/install-local.sh [--debug] [--no-build] [--dir DIR]
set -euo pipefail

profile=release
build=1
install_dir="$HOME/.local/bin"

usage() {
  cat <<'EOF'
用法：bash scripts/install-local.sh [选项]

把本地构建的 p4delta 装到安装目录（默认 $HOME/.local/bin），装完直接敲 p4delta 就能用。
这一步与 P4V 无关：往 P4V 里装是 Windows 侧的 install.ps1 / scripts/install-local.ps1。

选项：
  --debug       装 debug 构建。编译快、跑得慢，反复改代码时省编译时间。默认 release。
  --no-build    跳过 cargo build，直接装已有的构建产物。
  --dir DIR     安装目录，必须是绝对路径。默认 $HOME/.local/bin。
  -h, --help    显示这段帮助。
EOF
}

die() {
  echo "error: $*" >&2
  exit 1
}

# 用法错误与运行失败分开：前者连帮助一起打、退出码 2，被别的脚本串起来时好区分。
usage_error() {
  echo "error: $*" >&2
  echo >&2
  usage >&2
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --debug) profile=debug ;;
    --no-build) build=0 ;;
    --dir)
      shift
      # 不用 `shift 2`：只剩一个参数时它报的是 "shift count out of range"，
      # 指不到「你少给了一个值」。
      if [ $# -eq 0 ]; then
        usage_error "--dir 后面要跟一个目录。"
      fi
      install_dir="$1"
      ;;
    --dir=*) install_dir="${1#--dir=}" ;;
    -h | --help)
      usage
      exit 0
      ;;
    *) usage_error "不认识的参数：$1" ;;
  esac
  shift
done

# 去掉尾斜杠再校验。顺序（去尾斜杠 → 判非空 → 判绝对）不能换：`--dir /` 去完尾斜杠
# 就空了，得在那一步拦住。
install_dir="${install_dir%/}"
if [ -z "$install_dir" ]; then
  usage_error "--dir 是个空目录名。"
fi
# 要求绝对路径，是为了把两个静默坑变成一句能看懂的报错：`--dir=~/x` 里的 `~` 不会被
# 展开（波浪号只在**词首**展开，等号后面不算），而相对路径装到哪儿取决于你当时在哪个
# 目录——装完看着成功，下次在别处就找不到它。
case "$install_dir" in
  /*) ;;
  *) usage_error "--dir 必须是绝对路径：$install_dir" ;;
esac

# Git Bash / MSYS / Cygwin 的 uname 报的是 MINGW*/MSYS*/CYGWIN*，那边的 HOME 是个虚拟
# 目录，装过去等于白装：那边要用的是把 exe 装进 P4V 的 install-local.ps1。
case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*)
    die "这是 Git Bash / MSYS 环境，不是 WSL 或 Linux 终端。要把 p4delta 装进 P4V 请用 scripts/install-local.ps1。"
    ;;
esac

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# 构建产物在哪。CARGO_TARGET_DIR 设了就跟着它走：cargo 对相对值是按**运行目录**解析的，
# 而我们就是在仓库根跑 cargo，所以相对值也拿仓库根拼。认它不是可选的好习惯——本项目
# docs/dev/environment.md 的 WSL 一节就建议把它指到家目录（绕开 /mnt/* 的慢速），
# 不认的话构建会成功、紧接着报一句莫名其妙的「产物不存在」。
target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
case "$target_dir" in
  /*) ;;
  *) target_dir="$repo_root/$target_dir" ;;
esac
exe_path="$target_dir/$profile/p4delta"

if [ "$build" -eq 1 ]; then
  if ! command -v cargo >/dev/null 2>&1; then
    die "没找到 cargo：装好 Rust 工具链并把它放进 PATH，或者用 --no-build 装已有的产物。"
  fi
  # 子 shell 里 cd：cargo 靠当前目录向上找 .cargo/config.toml，同时不把脚本自己的 cwd
  # 挪走（下面还要用相对的 cwd 吗？不，但挪走 cwd 迟早坑到后来加的代码）。
  # 写成 if/else 而不是 `[ ... ] && cargo ...`：后者作为尾语句失败时会让 errexit 退出，
  # 而它看起来只是一句条件判断。
  if [ "$profile" = release ]; then
    echo "cargo build --release（在 $repo_root）"
    (cd "$repo_root" && cargo build --release)
  else
    echo "cargo build（在 $repo_root）"
    (cd "$repo_root" && cargo build)
  fi
else
  echo "跳过构建（--no-build），直接用 $exe_path"
fi

if [ ! -f "$exe_path" ]; then
  die "构建产物不存在：$exe_path。要么先跑一次构建（--debug 会找 debug 档的产物），要么去掉 --no-build。"
fi

mkdir -p "$install_dir"
# 规范化成绝对路径再往后走：下面要拿它逐字符比对 PATH 里的条目，不规范化的话
# `--dir ~/.local/bin` 这种写法永远匹配不上。
install_dir="$(cd "$install_dir" && pwd)"
target_exe="$install_dir/p4delta"

# 目标名已经被一个**目录**占着时必须停下：`mv -f 临时文件 目录` 不会报错，而是把文件
# 挪进那个目录里（变成 `$target_exe/p4delta`），脚本照样打印「已装到 …」，人要到敲命令
# 时才发现。`-d` 跟随符号链接，所以指向目录的链接也一并拦住。
# 这里不替人删：那是别人的东西，脚本只该动自己装的那一份。
if [ -d "$target_exe" ]; then
  die "$target_exe 是个目录，装不了。先自己把它处理掉（mv 或 rm）。"
fi

# 装：**先拷成同目录下的临时名，再改名到位**，而不是直接往目标文件上写。
#   - 同目录 rename 在同一个文件系统内是原子的，改名那一刻之前，别人看到的都是旧的那份；
#   - p4delta 正在运行时直接覆盖会 ETXTBSY（旧 inode 还在被内核执行），拷完再改名压根
#     不碰旧 inode——正在跑的进程继续用它手里那份，新起的进程拿到新的；
#   - 临时名交给 mktemp（O_EXCL），并发跑两份不会互相踩；也顺手给出 0600，逼着我们显式
#     想起权限这回事（见下面的 install -m）。
# 被 kill -9 时 trap 跑不到，目录里会剩一个 .p4delta.XXXXXX——那是改名之前的垃圾，
# 不是半个二进制，随手删掉即可。
tmp=
cleanup() {
  if [ -n "$tmp" ]; then
    rm -f "$tmp"
  fi
}
trap cleanup EXIT

tmp="$(mktemp "$install_dir/.p4delta.XXXXXX")"
# install -m 而不是 cp：cp 到**已存在**的目标不改权限位（POSIX，GNU cp 只在新建目标时套
# umask），mktemp 造的 0600 会原样留着——装出一个不可执行的二进制进 PATH，脚本当时还是绿的。
install -m 0755 "$exe_path" "$tmp"

# 冒烟跑在**待发布的那一份**上，不是跑在构建产物上：这样 noexec 挂载、拷贝被截断、
# 权限没设对这三种情况都在改名之前失败，坏的二进制永远进不了 PATH。装一份起不来的
# 东西进 PATH，症状会拖到真用它的时候才出现，在这里跑一次只要几十毫秒。
# 拿不住输出（比如这份构建连版本都打不出来）也算失败。
# （`p4delta --version` 在 clap 解析参数时就返回，不碰 p4/环境探测，所以在这儿跑与在
# 别处跑没有行为差异。）
if ! version="$("$tmp" --version 2>&1)" || [ -z "$version" ]; then
  die "冒烟失败，这份构建不能用：$tmp --version 输出「$version」"
fi

mv -f "$tmp" "$target_exe"   # rename(2)：到这一步才算装上了
tmp=                         # 已经改名走了，别让 trap 去删这个名字（可能已被别人复用）

# 这份构建对应哪份代码。本地构建与发布版版本号一样，光看版本号分不出「我验的到底是不是
# 刚才那次修改」，所以有未提交改动时必须显眼——与 install-local.ps1 的 Get-SourceRevision
# 同一套判据（status --porcelain 也认未跟踪文件，git describe --dirty 不认）。
# git 不在、或者这里不是 git 仓库，都只是少打一段信息，不是错误。
revision=''
if command -v git >/dev/null 2>&1; then
  if rev="$(git -C "$repo_root" rev-parse --short HEAD 2>/dev/null)"; then
    if [ -n "$(git -C "$repo_root" status --porcelain 2>/dev/null)" ]; then
      revision="$rev（有未提交改动）"
    else
      revision="$rev"
    fi
  fi
fi

echo "已装到 $target_exe"
if [ -n "$revision" ]; then
  echo "本地构建：$version  $revision"
else
  echo "本地构建：$version"
fi

# PATH 只提示、不改：Ubuntu 的 ~/.profile 默认就把 ~/.local/bin 加进去了，绝大多数人
# 看不到这句话。脚本去写 shell 配置是件出问题时没人查得到是谁干的事，不值得。
case ":$PATH:" in
  *":$install_dir:"*) ;;
  *)
    echo "提示：$install_dir 不在 PATH 里。把它加进你的 shell 配置（例如 ~/.profile）才能直接敲 p4delta。"
    ;;
esac

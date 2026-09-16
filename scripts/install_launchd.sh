#!/bin/bash
# Build, install, check, and restore the Rust launchd deployment.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"
HOME_CODEX="$HOME/.codex"
BACKUP_ROOT="$HOME_CODEX/ox-sse-proxy-backups"
LABEL="${OX_PROXY_LABEL:-com.ox-sse-proxy}"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
CONTROL="$HOME_CODEX/ox_sse_proxy.sh"
WRAPPER="$HOME_CODEX/ox_sse_proxy_launchd.sh"
BINARY="$HOME_CODEX/ox-sse-proxy"
BACKUP_DIR=""

die() {
  echo "error: $*" >&2
  exit 1
}

valid_label() {
  [[ "$1" =~ ^[A-Za-z0-9][A-Za-z0-9.-]*$ && "$1" != *..* && "$1" != *. ]]
}

# Reject labels that could escape the LaunchAgents directory or inject sed args.
valid_label "$LABEL" \
  || die "非法 OX_PROXY_LABEL: ${LABEL} (只允许字母、数字、点和连字符，且不能以点结尾或含连续点)"

on_exit() {
  local code=$?
  if (( code != 0 )) && [[ -n "$BACKUP_DIR" ]]; then
    echo "安装/启动失败；备份目录：$BACKUP_DIR" >&2
    echo "恢复命令：$0 restore '$BACKUP_DIR'" >&2
  fi
  exit "$code"
}
trap on_exit EXIT

target_path() {
  case "$1" in
    binary) printf '%s\n' "$BINARY" ;;
    control) printf '%s\n' "$CONTROL" ;;
    wrapper) printf '%s\n' "$WRAPPER" ;;
    plist) printf '%s\n' "$PLIST" ;;
    *) die "unknown manifest target: $1" ;;
  esac
}

backup_name() {
  case "$1" in
    binary) printf '%s\n' "ox-sse-proxy" ;;
    control) printf '%s\n' "ox_sse_proxy.sh" ;;
    wrapper) printf '%s\n' "ox_sse_proxy_launchd.sh" ;;
    plist) printf '%s\n' "$LABEL.plist" ;;
    *) die "unknown manifest target: $1" ;;
  esac
}

write_manifest_entry() {
  local name="$1" target="$2" backup="$3"
  if [[ -e "$target" || -L "$target" ]]; then
    cp -p "$target" "$BACKUP_DIR/$backup"
    printf '%s\tpresent\t%s\n' "$name" "$backup" >> "$BACKUP_DIR/manifest.tsv"
  else
    printf '%s\tabsent\t-\n' "$name" >> "$BACKUP_DIR/manifest.tsv"
  fi
}

make_backup() {
  mkdir -p "$BACKUP_ROOT"
  local stamp candidate
  stamp="$(date +%Y%m%d%H%M%S)"
  candidate="$BACKUP_ROOT/$stamp"
  [[ ! -e "$candidate" ]] || candidate="$BACKUP_ROOT/${stamp}-$$"
  mkdir -p "$candidate"
  BACKUP_DIR="$candidate"
  printf 'ox-sse-proxy backup v1\n' > "$BACKUP_DIR/manifest.tsv"
  write_manifest_entry binary "$BINARY" "$(backup_name binary)"
  write_manifest_entry control "$CONTROL" "$(backup_name control)"
  write_manifest_entry wrapper "$WRAPPER" "$(backup_name wrapper)"
  write_manifest_entry plist "$PLIST" "$(backup_name plist)"
  echo "备份已创建：$BACKUP_DIR"
}

legacy_labels() {
  local plist label
  for plist in "$HOME"/Library/LaunchAgents/*ox-sse-proxy*.plist; do
    [[ -f "$plist" && ! -L "$plist" ]] || continue
    label="$(basename "$plist" .plist)"
    [[ "$label" != "$LABEL" ]] || continue
    # Only retire plists that drive this project's launchd wrapper.
    grep -q 'ox_sse_proxy_launchd\.sh' "$plist" 2>/dev/null || continue
    printf '%s\n' "$label"
  done
}

stop_old_job() {
  local legacy
  while IFS= read -r legacy; do
    [[ -n "$legacy" ]] || continue
    if launchctl print "gui/$(id -u)/$legacy" >/dev/null 2>&1; then
      echo "停用遗留 label job：$legacy"
      launchctl bootout "gui/$(id -u)/$legacy" 2>/dev/null \
        || launchctl unload "$HOME/Library/LaunchAgents/$legacy.plist" 2>/dev/null \
        || true
    fi
  done < <(legacy_labels)
  if launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; then
    if ! launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null; then
      # The job can disappear between print and bootout. Only use the legacy
      # unload fallback while it is still observable as loaded.
      if launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; then
        launchctl unload "$PLIST" 2>/dev/null || {
          launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1 \
            || return 0
          die "旧 launchd job 无法卸载"
        }
      else
        return 0
      fi
    fi
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      if ! launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; then
        return 0
      fi
      sleep 0.1
    done
    die "旧 launchd job 仍处于 loaded 状态"
  fi
}

render_template() {
  local template="$1" output="$2"
  local escaped_home="$HOME" escaped_label="$LABEL"
  escaped_home="${escaped_home//\\/\\\\}"
  escaped_home="${escaped_home//&/\\&}"
  escaped_home="${escaped_home//|/\\|}"
  escaped_label="${escaped_label//&/\\&}"
  escaped_label="${escaped_label//|/\\|}"
  sed \
    -e "s|__HOME__|$escaped_home|g" \
    -e "s|__LABEL__|$escaped_label|g" \
    "$template" > "$output"
}

render_plist() {
  render_template "$SCRIPT_DIR/com.ox-sse-proxy.plist.in" "$1"
}

validate_rendered_plist() {
  local rendered="$1"
  command -v plutil >/dev/null 2>&1 || die "未找到 plutil，无法校验 rendered plist"
  plutil -lint "$rendered" >/dev/null || die "rendered plist 校验失败：$rendered"
}

validate_plist_template() {
  local rendered
  rendered="$(mktemp "${TMPDIR:-/tmp}/ox-sse-proxy-plist.XXXXXX")"
  render_plist "$rendered"
  validate_rendered_plist "$rendered"
  rm -f "$rendered"
}

atomic_install() {
  local stage
  stage="$(mktemp -d "$HOME_CODEX/.ox-sse-proxy-install.XXXXXX")"
  trap 'rm -rf "$stage"' RETURN
  install -m 755 "$REPO_ROOT/target/release/ox-sse-proxy" "$stage/ox-sse-proxy"
  render_template "$SCRIPT_DIR/ox_sse_proxy.sh" "$stage/ox_sse_proxy.sh"
  chmod 755 "$stage/ox_sse_proxy.sh"
  install -m 755 "$SCRIPT_DIR/ox_sse_proxy_launchd.sh" "$stage/ox_sse_proxy_launchd.sh"
  render_plist "$stage/$LABEL.plist"
  validate_rendered_plist "$stage/$LABEL.plist"
  chmod 644 "$stage/$LABEL.plist"
  mkdir -p "$HOME_CODEX" "$(dirname "$PLIST")"
  mv -f "$stage/ox-sse-proxy" "$BINARY"
  mv -f "$stage/ox_sse_proxy.sh" "$CONTROL"
  mv -f "$stage/ox_sse_proxy_launchd.sh" "$WRAPPER"
  mv -f "$stage/$LABEL.plist" "$PLIST"
  rmdir "$stage"
  trap - RETURN
}

install_release() {
  echo "构建 Rust release binary..."
  (cd "$REPO_ROOT" && cargo build --release)
  [[ -x "$REPO_ROOT/target/release/ox-sse-proxy" ]] || die "release binary 未生成"
  make_backup
  # Fail before stopping the existing job or replacing any target if the
  # template cannot render into a valid plist.
  validate_plist_template
  stop_old_job
  atomic_install
  "$CONTROL" start
  "$CONTROL" check
  echo "安装完成；如需回滚：$0 restore '$BACKUP_DIR'"
}

check_release() {
  [[ -x "$CONTROL" ]] || die "未找到已安装控制脚本：$CONTROL"
  "$CONTROL" check
}

resolve_backup_dir() {
  local requested="${1:-latest}" root_real parent_real base candidate
  root_real="$(cd "$BACKUP_ROOT" 2>/dev/null && pwd -P)" || die "backup root 不存在：$BACKUP_ROOT"
  if [[ "$requested" == "latest" ]]; then
    candidate="$(find "$root_real" -mindepth 1 -maxdepth 1 -type d -print | sort | tail -n 1)"
    [[ -n "$candidate" ]] || die "没有可恢复的备份"
  else
    if [[ "$requested" = /* ]]; then
      candidate="$requested"
    else
      candidate="$PWD/$requested"
    fi
  fi
  parent_real="$(cd "$(dirname "$candidate")" 2>/dev/null && pwd -P)" || die "无效备份路径：$requested"
  base="$(basename "$candidate")"
  [[ "$parent_real" == "$root_real" && "$base" != "." && "$base" != ".." ]] || die "备份路径必须是 backup root 的直接子目录"
  candidate="$root_real/$base"
  [[ -d "$candidate" && ! -L "$candidate" ]] || die "备份目录不存在或是符号链接：$candidate"
  printf '%s\n' "$candidate"
}

restore_release() {
  local dir="$1" manifest="$1/manifest.tsv" name state backup target seen=""
  [[ -f "$manifest" ]] || die "缺少 manifest：$manifest"
  [[ "$(sed -n '1p' "$manifest")" == "ox-sse-proxy backup v1" ]] || die "不支持的 manifest"
  while IFS=$'\t' read -r name state backup; do
    [[ -n "$name" ]] || continue
    case "$name" in binary|control|wrapper|plist) ;; *) die "manifest target 非法：$name" ;; esac
    [[ "$seen" != *"|$name|"* ]] || die "manifest target 重复：$name"
    seen="$seen|$name|"
    target="$(target_path "$name")"
    case "$state" in
      present)
        if [[ "$name" == plist ]]; then
          # The plist file name follows the install-time label, which can differ
          # from OX_PROXY_LABEL after a rename or a cross-version rollback.
          [[ "$backup" == *.plist && "$backup" != */* && -f "$dir/$backup" ]] \
            || die "manifest plist backup 非法：$backup"
        else
          [[ "$backup" == "$(backup_name "$name")" && -f "$dir/$backup" ]] \
            || die "manifest backup 非法：$name"
        fi
        ;;
      absent) [[ "$backup" == "-" ]] || die "manifest absent 条目非法：$name" ;;
      *) die "manifest state 非法：$name" ;;
    esac
  done < <(sed -n '2,$p' "$manifest")
  for name in binary control wrapper plist; do
    [[ "$seen" == *"|$name|"* ]] || die "manifest 缺少 target：$name"
  done

  stop_old_job
  while IFS=$'\t' read -r name state backup; do
    target="$(target_path "$name")"
    if [[ "$state" == "present" ]]; then
      install -m "$([[ "$name" == binary || "$name" == control || "$name" == wrapper ]] && echo 755 || echo 644)" "$dir/$backup" "$target"
    else
      [[ ! -e "$target" && ! -L "$target" ]] || rm -f "$target"
    fi
  done < <(sed -n '2,$p' "$manifest")
  if [[ -f "$PLIST" ]]; then
    launchctl bootstrap "gui/$(id -u)" "$PLIST" 2>/dev/null || launchctl load "$PLIST"
    echo "已恢复并重新加载原 plist：$PLIST"
  else
    echo "原 plist 不存在，未重新加载 launchd job"
  fi
  if [[ -x "$CONTROL" ]]; then
    "$CONTROL" status || true
  elif [[ -f "$PLIST" ]]; then
    launchctl print "gui/$(id -u)/$LABEL" 2>/dev/null || true
  else
    echo "恢复后控制脚本原本不存在"
  fi
}

case "${1:-}" in
  install) install_release ;;
  check) check_release ;;
  restore)
    resolved_backup="$(resolve_backup_dir "${2:-latest}")"
    restore_release "$resolved_backup"
    ;;
  *) echo "usage: $0 {install|check|restore [backup-dir|latest]}" >&2; exit 1 ;;
esac

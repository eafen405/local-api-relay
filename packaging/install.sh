#!/usr/bin/env bash
# install.sh — idempotent user-level installer for the self-contained
# local-api-relay archive (PKG-002/003/004).
#
# The archive ships exactly three files: the `local-api-relay` binary, this
# installer, and the `local-api-relay-service` lifecycle script. Installing
# requires no package repository, no root-owned system directories, no
# container runtime, no Node.js, and no desktop shell.
#
# Layout (PKG-003/004):
#   ~/.local/opt/local-api-relay/<version>/bin/local-api-relay   versioned program files
#   ~/.local/bin/local-api-relay                                  stable user-level entry (symlink)
#   ~/.local/bin/local-api-relay-service                          lifecycle commands
#   $XDG_DATA_HOME/local-api-relay/                               SQLite database + backups
#   $XDG_CONFIG_HOME/local-api-relay/                             process configuration
#   $XDG_STATE_HOME/local-api-relay/                              runtime state + logs
#
# Every directory and secret-bearing file in the layout is owner-only.
# Re-running the installer for the same version is a safe no-op that
# re-ensures the layout; installing a new version performs the upgrade flow
# (PKG-013): it keeps the previous version side by side, verifies the new
# binary against a staged copy of the database before anything switches,
# creates and verifies the pre-migration backup when one is needed, atomically
# switches the stable entry, and restarts the scheduled task / service. A
# failed upgrade never modifies the live database and can be recovered with
# `local-api-relay-service rollback` (PKG-014).
set -eu

readonly APP_NAME="local-api-relay"

home_dir="${HOME:-}"
if [ -z "$home_dir" ]; then
    echo "install: HOME is required" >&2
    exit 1
fi

# Resolve the directory this installer was unpacked into.
archive_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
binary="$archive_dir/$APP_NAME"
service_script="$archive_dir/local-api-relay-service"

if [ ! -f "$binary" ]; then
    echo "install: $APP_NAME binary is missing from the archive" >&2
    exit 1
fi
if [ ! -f "$service_script" ]; then
    echo "install: local-api-relay-service is missing from the archive" >&2
    exit 1
fi

version=$("$binary" --version 2>/dev/null | awk '{print $2}')
if [ -z "$version" ]; then
    echo "install: could not determine the binary version" >&2
    exit 1
fi

install_root="$home_dir/.local"
versioned_dir="$install_root/opt/$APP_NAME/$version/bin"
bin_dir="$install_root/bin"
entry="$bin_dir/$APP_NAME"
versioned_binary="$versioned_dir/$APP_NAME"

# XDG application directories (PKG-003); the service binary uses the same
# resolution when the XDG variables are unset.
data_dir="${XDG_DATA_HOME:-$home_dir/.local/share}/$APP_NAME"
config_dir="${XDG_CONFIG_HOME:-$home_dir/.config}/$APP_NAME"
state_dir="${XDG_STATE_HOME:-$home_dir/.local/state}/$APP_NAME"

mkdir -p "$versioned_dir" "$bin_dir" "$data_dir" "$config_dir" "$state_dir"
chmod 700 "$versioned_dir" "$bin_dir" "$data_dir" "$config_dir" "$state_dir"
chmod 700 "$install_root" "$install_root/opt" "$install_root/opt/$APP_NAME" 2>/dev/null || true

# The Windows login-task name, resolved once here so both the task creation
# section and the upgrade/rollback lifecycle use the same name.
windows_task_name="${LOCAL_API_RELAY_WINDOWS_TASK_NAME:-$APP_NAME}"

# ---------------------------------------------------------------------------
# Upgrade helpers (PKG-013/PKG-014)
# ---------------------------------------------------------------------------
# The upgrade state records which version the stable entry previously selected
# and which pre-migration backup the rollback must restore; it is written by
# install.sh and consumed by `local-api-relay-service rollback`.
upgrade_state="$state_dir/upgrade.state"

# The configured loopback port, the same rule the lifecycle script and the
# launcher use and that must stay in sync with them (PKG-009).
configured_port() {
    local port="8787"
    if [ -f "$config_dir/service.json" ]; then
        local configured
        configured=$(sed -n 's/.*"port"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$config_dir/service.json" | head -n 1)
        [ -n "$configured" ] && port="$configured"
    fi
    printf '%s' "$port"
}

# Single-shot ready probe on the configured port: exactly HTTP 200 is ready.
# This mirrors the probe embedded in the lifecycle script and the launcher and
# must stay in sync with them. The comparison lives inside an `if` so a
# not-ready probe can return non-zero without tripping `set -e` (a failing
# last statement in a function would abort the whole script).
serving_now() {
    local port
    port=$(configured_port)
    { exec 9<>"/dev/tcp/127.0.0.1/$port"; } 2>/dev/null || return 1
    printf 'GET /ready HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n' >&9
    local first_line=""
    IFS= read -r -t 2 first_line <&9 || true
    # Close the probe descriptor in a subshell: a bare `exec ... 2>/dev/null`
    # would permanently redirect this shell's own stderr, silencing every
    # later diagnostic.
    (exec 9<&- 9>&-) 2>/dev/null || true
    set -- $first_line
    if [ "${2:-}" = "200" ]; then
        return 0
    fi
    return 1
}

# Waits until the relay answers ready on the configured port, bounded.
wait_ready() {
    local timeout_ms="$1" now_ms deadline_ms
    deadline_ms=$(( $(date +%s%N 2>/dev/null) / 1000000 + timeout_ms ))
    while :; do
        if serving_now; then
            return 0
        fi
        now_ms=$(date +%s%N 2>/dev/null)
        now_ms=${now_ms%??????}
        [ "${now_ms:-0}" -ge "$deadline_ms" ] && return 1
        sleep 0.2
    done
}

# Waits until the configured port stops answering ready, bounded.
wait_not_serving() {
    local timeout_ms="$1" now_ms deadline_ms
    deadline_ms=$(( $(date +%s%N 2>/dev/null) / 1000000 + timeout_ms ))
    while :; do
        if ! serving_now; then
            return 0
        fi
        now_ms=$(date +%s%N 2>/dev/null)
        now_ms=${now_ms%??????}
        [ "${now_ms:-0}" -ge "$deadline_ms" ] && return 1
        sleep 0.2
    done
}

# Whether the per-user Windows login task is registered. The upgrade and the
# lifecycle restart use the task when it exists and the hermetic skip hook is
# not set; otherwise they use the lifecycle service script.
task_registered() {
    [ "${LOCAL_API_RELAY_WINDOWS_TASK_SKIP:-}" = "1" ] && return 1
    command -v schtasks.exe >/dev/null 2>&1 || return 1
    schtasks.exe /Query /TN "$windows_task_name" >/dev/null 2>&1
}

stop_service() {
    # A task-managed serve has no pidfile, so end the task when it exists; the
    # lifecycle stop covers a pidfile-managed serve (and is a harmless no-op
    # otherwise).
    if task_registered; then
        schtasks.exe /End /TN "$windows_task_name" >/dev/null 2>&1 || true
    fi
    "$bin_dir/local-api-relay-service" stop >/dev/null 2>&1 || true
}

start_service() {
    if task_registered; then
        schtasks.exe /Run /TN "$windows_task_name" >/dev/null 2>&1 || true
    else
        "$bin_dir/local-api-relay-service" start >/dev/null 2>&1 || true
    fi
}

# Atomically replaces the stable user-level entry so a concurrent reader never
# sees a half-written link.
switch_entry() {
    local target="$1"
    local tmp_link="$bin_dir/.$APP_NAME.tmp.$$"
    ln -sfn "$target" "$tmp_link"
    mv -f "$tmp_link" "$entry"
}

# Stages a private copy of the live database and trial-starts the new binary
# against it on the configured port: proves the binary runs, the process
# configuration is compatible, the embedded management assets are served, the
# startup preconditions hold (the configured port binds), and any forward
# migration succeeds on the copy — all without touching the live database
# (PKG-013). The trial directory is owner-only and removed on every exit.
trial_serve() {
    local trial_root="$state_dir/upgrade-trial"
    rm -rf "$trial_root"
    mkdir -p "$trial_root/xdg-data/$APP_NAME" "$trial_root/xdg-state"
    chmod 700 "$trial_root" "$trial_root/xdg-data" "$trial_root/xdg-state" 2>/dev/null || true
    local trial_log="$trial_root/trial.log"
    # The trap cleans up the staged copy on every return path and disarms
    # itself: bash leaks a RETURN trap into the calling function, where it
    # would fire again with `trial_root` out of scope and abort the script.
    trap 'rm -rf "$trial_root"; trap - RETURN' RETURN

    local trial_db="$trial_root/xdg-data/$APP_NAME/relay.sqlite3"
    if [ -f "$data_dir/relay.sqlite3" ]; then
        cp "$data_dir/relay.sqlite3" "$trial_db"
        chmod 600 "$trial_db" 2>/dev/null || true
        # The staged copy must be as consistent as the live file: when the
        # service was stopped by the task path the WAL may not be checkpointed,
        # so carry any -wal/-shm sidecars along.
        if [ -f "$data_dir/relay.sqlite3-wal" ]; then
            cp "$data_dir/relay.sqlite3-wal" "$trial_db-wal"
            chmod 600 "$trial_db-wal" 2>/dev/null || true
        fi
        if [ -f "$data_dir/relay.sqlite3-shm" ]; then
            cp "$data_dir/relay.sqlite3-shm" "$trial_db-shm"
            chmod 600 "$trial_db-shm" 2>/dev/null || true
        fi
    fi

    env XDG_DATA_HOME="$trial_root/xdg-data" XDG_STATE_HOME="$trial_root/xdg-state" \
        "$versioned_binary" serve >>"$trial_log" 2>&1 &
    local trial_pid=$!

    if ! wait_ready 10000; then
        kill -TERM "$trial_pid" 2>/dev/null || true
        sleep 1
        kill -KILL "$trial_pid" 2>/dev/null || true
        echo "install: upgrade preflight failed: the new binary did not become ready on port $(configured_port)" >&2
        echo "install: inspect $trial_log" >&2
        return 1
    fi

    # The new binary must serve the embedded management page (PKG-013).
    local port
    port=$(configured_port)
    local page_ok=false
    if { exec 9<>"/dev/tcp/127.0.0.1/$port"; } 2>/dev/null; then
        printf 'GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n' >&9
        local page=""
        local line=""
        while IFS= read -r -t 2 line <&9; do
            page="$page$line"
        done
        (exec 9<&- 9>&-) 2>/dev/null || true
        case "$page" in
            *"本地 API 中转"*) page_ok=true ;;
        esac
    fi
    if [ "$page_ok" != "true" ]; then
        kill -TERM "$trial_pid" 2>/dev/null || true
        sleep 1
        kill -KILL "$trial_pid" 2>/dev/null || true
        echo "install: upgrade preflight failed: the new binary did not serve the embedded management page" >&2
        return 1
    fi

    kill -TERM "$trial_pid" 2>/dev/null || true
    local deadline=$(( $(date +%s) + 10 ))
    while kill -0 "$trial_pid" 2>/dev/null && [ "$(date +%s)" -lt "$deadline" ]; do
        sleep 0.1
    done
    kill -KILL "$trial_pid" 2>/dev/null || true
    return 0
}

# Restores the previously running service after a pre-switch upgrade failure:
# the stable entry has not moved, so the previous version still serves.
restore_service_after_failure() {
    if [ "$was_serving" = "true" ]; then
        echo "install: upgrade failed before the entry was switched; restoring the previous service" >&2
        start_service
        if ! wait_ready 15000; then
            echo "install: warning: the previous service could not be restarted" >&2
            echo "install:   run: $bin_dir/local-api-relay-service status" >&2
        fi
    fi
}

# Switches the stable entry straight back to the previous version after a
# restart failure. Only used when no forward migration committed, so the live
# database is untouched and the previous binary reads it as before (PKG-014).
direct_rollback() {
    echo "install: upgrade restart failed; switching the stable entry back to $previous_version" >&2
    switch_entry "$install_root/opt/$APP_NAME/$previous_version/bin/$APP_NAME"
    rm -f "$upgrade_state"
    if [ "$was_serving" = "true" ]; then
        start_service
        if ! wait_ready 15000; then
            echo "install: warning: the previous service could not be restarted" >&2
            echo "install:   run: $bin_dir/local-api-relay-service status" >&2
        fi
    fi
    echo "install: error: the upgrade to $version failed and was rolled back to $previous_version" >&2
    exit 1
}

# The upgrade orchestration (PKG-013/PKG-014): stop the running service, verify
# the new binary against a staged copy, create and verify the migration
# pre-backup when one is needed, record the upgrade state, switch the stable
# entry atomically, and restart the scheduled task / lifecycle service. Any
# pre-switch failure leaves the entry and the live database untouched and
# restores the previous service.
upgrade_flow() {
    was_serving=false
    if serving_now; then
        was_serving=true
    fi

    # Stop the running service so the database is quiescent for verification
    # and the configured port is free for the trial serve.
    if [ "$was_serving" = "true" ]; then
        stop_service
        wait_not_serving 10000 || true
    fi

    # 1) Pre-flight check: the binary runs, the process configuration parses,
    #    and the live database schema is supported or migratable.
    local check_out check_rc
    set +e
    check_out=$("$versioned_binary" check 2>&1)
    check_rc=$?
    set -e
    if [ "$check_rc" -ne 0 ]; then
        echo "install: upgrade preflight failed: the new version $version is not compatible with this install" >&2
        printf '%s\n' "$check_out" | sed 's/^/install:   /' >&2
        restore_service_after_failure
        exit 1
    fi
    supported_schema=$(printf '%s\n' "$check_out" | sed -n 's/^supported_schema=//p' | head -n 1)
    database_schema=$(printf '%s\n' "$check_out" | sed -n 's/^database_schema=//p' | head -n 1)
    migration_needed=$(printf '%s\n' "$check_out" | sed -n 's/^migration_needed=//p' | head -n 1)

    # 2) Trial serve on a staged copy: startup preconditions, embedded assets,
    #    and (when required) the forward migration all prove out before the
    #    switch. Skippable only through the test hook.
    if [ "${LOCAL_API_RELAY_UPGRADE_SKIP_TRIAL:-}" != "1" ]; then
        if ! trial_serve; then
            restore_service_after_failure
            exit 1
        fi
    fi

    # 3) Pre-migration backup of the live database when a forward migration is
    #    required: it must be created and verified before the stable entry is
    #    switched; a failure here keeps the old entry and never modifies the
    #    live database (PKG-013).
    pre_backup_name=""
    if [ "$migration_needed" = "true" ]; then
        local backup_out backup_rc
        set +e
        backup_out=$("$versioned_binary" backup --reason migration 2>&1)
        backup_rc=$?
        set -e
        if [ "$backup_rc" -ne 0 ]; then
            echo "install: upgrade preflight failed: the pre-migration backup could not be created and verified" >&2
            printf '%s\n' "$backup_out" | sed 's/^/install:   /' >&2
            restore_service_after_failure
            exit 1
        fi
        pre_backup_name=$(printf '%s\n' "$backup_out" | sed -n 's/^backup=//p' | head -n 1)
    fi

    # 4) Record the upgrade state for `local-api-relay-service rollback`, then
    #    switch the stable executable entry atomically. The previous program
    #    version stays installed side by side (PKG-013).
    {
        printf 'previous_version=%s\n' "$previous_version"
        if [ -n "$pre_backup_name" ]; then
            printf 'pre_backup=%s\n' "$pre_backup_name"
        fi
    } >"$upgrade_state"
    chmod 600 "$upgrade_state"
    switch_entry "$versioned_binary"

    # 5) Restart the service (only when it was serving) and wait for ready; the
    #    client address and the management entry stay on the stable port.
    if [ "$was_serving" = "true" ]; then
        start_service
        if ! wait_ready 20000; then
            # Restart failed after the switch. With no committed forward
            # migration the live database is untouched and the entry can switch
            # straight back; once a forward migration committed, only the
            # previous binary with an explicit restore of the migration
            # pre-backup can repair the database (PKG-014) — never a downgrade.
            if [ "$migration_needed" = "true" ]; then
                local post_check post_schema
                set +e
                post_check=$("$versioned_binary" check 2>&1)
                set -e
                post_schema=$(printf '%s\n' "$post_check" | sed -n 's/^database_schema=//p' | head -n 1)
                # The migration is atomic, so the live schema is either still
                # below the new binary's supported version (it rolled back) or
                # exactly at it (it committed). Only a proven-uncommitted
                # migration may switch the entry straight back; anything else —
                # the schema reached the supported version, or the probe itself
                # failed — must keep the new entry and route the recovery
                # through the explicit rollback, which restores the
                # pre-migration backup with the previous binary (PKG-014).
                if [ -z "$post_schema" ] || [ "$post_schema" = "none" ] || [ "$post_schema" -ge "$supported_schema" ]; then
                    echo "install: error: the upgrade to $version failed and the forward migration state cannot be rolled back directly" >&2
                    echo "install: the stable entry stays on $version; the live database is schema ${post_schema:-unknown}" >&2
                    echo "install: roll back with: $bin_dir/local-api-relay-service rollback" >&2
                    exit 1
                fi
            fi
            direct_rollback
        fi
        echo "install: upgraded to $version and restarted the service on port $(configured_port)"
    else
        echo "install: upgraded to $version (the service was not running; start it with $bin_dir/local-api-relay-service start)"
    fi
}

# Versioned binary, owner-only (PKG-004). Hardlink when the archive lives on
# the same filesystem so repeated installs stay cheap; otherwise copy.
install_file() {
    local source="$1" target="$2"
    if ! cp -l "$source" "$target" 2>/dev/null; then
        install -m 700 "$source" "$target"
    fi
    chmod 700 "$target"
}

install_file "$binary" "$versioned_binary"

# Lifecycle script (PKG-007), owner-only; installed before the upgrade flow so
# it can stop and start the service around the switch.
install_file "$service_script" "$bin_dir/local-api-relay-service"

# ---------------------------------------------------------------------------
# Upgrade detection and orchestration (PKG-013/PKG-014)
# ---------------------------------------------------------------------------
# An install is an upgrade when the stable entry already selects a different
# version, or a different versioned program directory exists. The previous
# program version is always kept side by side so the entry can be switched
# back. A same-version reinstall stays a plain idempotent install.
previous_version=""
if [ -L "$entry" ]; then
    resolved=$(readlink -f "$entry" 2>/dev/null || true)
    if [ -n "$resolved" ]; then
        resolved_version=$(basename "$(dirname "$(dirname "$resolved")")" 2>/dev/null || true)
        if [ -n "$resolved_version" ]; then
            previous_version="$resolved_version"
        fi
    fi
fi
if [ -z "$previous_version" ]; then
    newest_version=$(ls -1 "$install_root/opt/$APP_NAME" 2>/dev/null | sort -V | tail -n 1 || true)
    [ -n "$newest_version" ] && previous_version="$newest_version"
fi

if [ -n "$previous_version" ] && [ "$previous_version" != "$version" ]; then
    upgrade_flow
else
    # Plain install (fresh or same-version reinstall): switch the stable entry
    # atomically so a concurrent reader never sees a half-written link.
    switch_entry "$versioned_binary"
fi

# ---------------------------------------------------------------------------
# Windows login task (PKG-005/006) and desktop console launcher (PKG-008).
# ---------------------------------------------------------------------------
#
# The installer registers a per-user Windows scheduled task that fires at the
# user's Windows logon and directly holds a long-running wsl.exe invocation
# of `local-api-relay serve`, keeping the WSL2 VM alive for as long as the
# relay runs (PKG-005). The task carries a bounded abnormal-exit restart
# policy (PKG-006), runs only in the interactive session of the installing
# user (never before Windows logon, no stored password — SEC-005), and does
# not depend on WSL systemd.
#
# schtasks.exe switch mode cannot express a per-user logon trigger or the
# restart policy, so the task is registered from an XML template
# (schtasks.exe /Create /XML), the representation verified to work for a
# standard user; see .scratch/local-api-relay-mvp/research/
# windows-login-task-and-console-launcher.md.
if [ "${LOCAL_API_RELAY_WINDOWS_TASK_SKIP:-}" = "1" ]; then
    echo "install: note: Windows login task skipped (LOCAL_API_RELAY_WINDOWS_TASK_SKIP=1)"
elif ! command -v schtasks.exe >/dev/null 2>&1; then
    echo "install: note: schtasks.exe not found; the Windows login task (PKG-005) is not created" >&2
else
    win_user=$(cmd.exe /c whoami 2>/dev/null | tr -d '\r\n')
    wsl_distro="${WSL_DISTRO_NAME:-}"
    wsl_user="${USER:-}"
    if [ -z "$win_user" ] || [ -z "$wsl_distro" ] || [ -z "$wsl_user" ]; then
        echo "install: error: could not determine the Windows user, WSL distribution, or WSL user; the Windows login task was not created" >&2
        exit 1
    fi
    xml_escape() {
        printf '%s' "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'
    }
    # The template is written UTF-16LE with a BOM, like schtasks.exe /Query
    # /XML emits, and passed to schtasks.exe through the \\wsl.localhost UNC
    # path so it is readable from Windows.
    task_xml="$state_dir/windows-login-task.xml"
    {
        printf '\xff\xfe'
        cat <<XML | iconv -f UTF-8 -t UTF-16LE
<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>$APP_NAME Windows login task</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <UserId>$(xml_escape "$win_user")</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>$(xml_escape "$win_user")</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure>
      <Count>3</Count>
      <Interval>PT1M</Interval>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>wsl.exe</Command>
      <Arguments>-d $(xml_escape "$wsl_distro") -u $(xml_escape "$wsl_user") -- $(xml_escape "$entry") serve</Arguments>
    </Exec>
  </Actions>
</Task>
XML
    } >"$task_xml"
    chmod 600 "$task_xml"
    unc_root="\\\\wsl.localhost\\$wsl_distro"
    state_win=$(printf '%s' "$state_dir" | sed 's|^/||; s|/|\\|g')
    task_xml_windows="$unc_root\\$state_win\\windows-login-task.xml"
    if schtasks.exe /Create /TN "$windows_task_name" /XML "$task_xml_windows" /F >/dev/null 2>&1; then
        rm -f "$task_xml"
        echo "  Windows login task:  $windows_task_name (per-user logon, bounded restart)"
    else
        rm -f "$task_xml"
        echo "install: error: could not create the Windows login task '$windows_task_name'" >&2
        echo "install: error: diagnose with: schtasks.exe /Query /TN \"$windows_task_name\" /V /FO LIST" >&2
        exit 1
    fi
fi

# Desktop console launcher (PKG-008), owner-only. It checks the dedicated
# local ready endpoint; when the relay is ready it opens the Windows default
# browser to the management page, otherwise it shows the service status and
# the actionable diagnostic commands. No secrets (SEC-005). The launcher is
# generated so the Windows login-task name is baked into its diagnostics.
launcher="$bin_dir/local-api-relay-launcher"
cat >"$launcher" <<'LAUNCHER'
#!/usr/bin/env bash
# local-api-relay-launcher — desktop console launcher (PKG-008).
#
# Checks the dedicated local ready endpoint (GET /ready); when the relay is
# ready it opens the Windows default browser to the management page,
# otherwise it shows the service status and the actionable diagnostic
# commands for this install. Carries no secrets (SEC-005).
#
# Exit codes: 0 ready, 1 not ready.
set -u

APP_NAME="local-api-relay"
WINDOWS_TASK_NAME="@WINDOWS_TASK_NAME@"

home_dir="${HOME:-}"
if [ -z "$home_dir" ]; then
    echo "local-api-relay-launcher: HOME is required" >&2
    exit 1
fi

config_home="${XDG_CONFIG_HOME:-$home_dir/.config}/$APP_NAME"
install_root="$home_dir/.local"
service_script="$install_root/bin/local-api-relay-service"

# The explicit process configuration chooses another stable port (PKG-009);
# without it the launcher checks the stable default. This port-parse rule and
# the /dev/tcp ready probe mirror local-api-relay-service and must stay in
# sync with that script.
port="8787"
if [ -f "$config_home/service.json" ]; then
    configured_port=$(sed -n 's/.*"port"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$config_home/service.json" | head -n 1)
    if [ -n "$configured_port" ]; then
        port="$configured_port"
    fi
fi

# A short ready probe: the launcher must not wait for a service that is
# starting up, it only distinguishes ready from anything else.
ready() {
    { exec 9<>"/dev/tcp/127.0.0.1/$port"; } 2>/dev/null || return 1
    printf 'GET /ready HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n' >&9
    local first_line=""
    IFS= read -r -t 2 first_line <&9 || true
    # Close the probe descriptor in a subshell so this shell's own stderr is
    # not permanently redirected by the bare `exec ... 2>/dev/null` form.
    (exec 9<&- 9>&-) 2>/dev/null || true
    # The ready contract is exactly HTTP 200 (PKG-010); a different code, a
    # proxy page, or an empty reply is not ready.
    set -- $first_line
    [ "${2:-}" = "200" ]
}

if ready; then
    management_url="http://127.0.0.1:$port/"
    if [ -n "${LOCAL_API_RELAY_LAUNCHER_NO_BROWSER:-}" ]; then
        echo "local-api-relay: ready on $management_url (browser suppressed by LOCAL_API_RELAY_LAUNCHER_NO_BROWSER)"
        exit 0
    fi
    if command -v cmd.exe >/dev/null 2>&1; then
        # `start` with the required empty title placeholder opens the URL in
        # the Windows default browser (URLs run through their association).
        cmd.exe /c start "" "$management_url"
        echo "local-api-relay: ready — opened $management_url in the default browser"
    else
        echo "local-api-relay: ready on $management_url — open this URL in your browser"
    fi
    exit 0
fi

echo "local-api-relay: not ready on 127.0.0.1:$port"
echo
echo "diagnostics:"
if [ -x "$service_script" ]; then
    "$service_script" status || true
fi
echo
echo "next steps:"
echo "  start the service now:   $service_script start"
echo "  inspect the login task:  schtasks.exe /Query /TN \"$WINDOWS_TASK_NAME\" /V /FO LIST"
echo "  inspect WSL health:      wsl.exe --status"
exit 1
LAUNCHER
# Escape sed replacement specials (`&`, `/`, `\`) so a task name from the
# LOCAL_API_RELAY_WINDOWS_TASK_NAME hook cannot corrupt the launcher file.
escaped_task_name=$(printf '%s' "$windows_task_name" | sed 's/[&/\\]/\\&/g')
sed -i "s/@WINDOWS_TASK_NAME@/$escaped_task_name/g" "$launcher"
chmod 700 "$launcher"

echo "installed $APP_NAME $version"
echo "  versioned programs: $install_root/opt/$APP_NAME/$version"
echo "  stable entry:       $entry"
echo "  lifecycle:          $bin_dir/local-api-relay-service {start|stop|restart|status|rollback}"
if [ -n "$previous_version" ] && [ "$previous_version" != "$version" ]; then
    echo "  previous version:   $install_root/opt/$APP_NAME/$previous_version (kept for rollback)"
    echo "  rollback:           $bin_dir/local-api-relay-service rollback"
fi
if ! printf '%s' "$PATH" | tr ':' '\n' | grep -Fqx "$bin_dir"; then
    echo "  note: add $bin_dir to PATH, for example:"
    echo "        export PATH=\"\$HOME/.local/bin:\$PATH\""
fi

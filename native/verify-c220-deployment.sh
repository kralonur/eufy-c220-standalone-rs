#!/bin/sh
set -eu

# Post-deployment gate for the standalone C220 camera.
#
# Why this exists: the firmware, the RTSP service and the media engine live on a
# read-only squashfs, so changing them means overwriting that partition on the
# camera's flash. That write path silently drops the last erase block of every
# write (see "Flashing the home partition on this board" in this directory), so a
# deployment is finished not when `dd` reports success but when the camera has
# rebooted, served its API, produced video and reported the hashes of the flash it
# actually booted. Every `/home` write ends with this script, and a failure here
# means the programmer, not another network write.
#
# The checks are layered so partial information is still useful:
#   * health and the port map need nothing but the network;
#   * /status and the RTSPS probe need the HTTP and RTSP credentials;
#   * the flash, process and log checks need the one-boot maintenance shell.
#
# The only thing this script changes on the camera is arming that shell, and it
# closes it again on the way out.

CAMERA=${C220_CAMERA:-192.168.50.55}
NVRAM_DIR=${C220_NVRAM:-}
IMAGE=${C220_IMAGE:-}
ENGINE_MD5=${C220_ENGINE_MD5:-}
FIRMWARE_MD5=${C220_FIRMWARE_MD5:-}
NETWORK_ONLY=${C220_NETWORK_ONLY:-0}

# The shell is reached through the API's digest-authenticated route, and the
# password files are read from the same directory as the config that names them.
HTTP_USER=
HTTP_PASSWORD=
RTSP_USER=
RTSP_PASSWORD=

# Erase block size of the board's SPI flash, from /proc/mtd on the camera.
ERASE_BLOCK_BYTES=65536

pass_count=0
failure_count=0
skip_count=0

usage() {
    cat <<'EOF'
usage: verify-c220-deployment.sh [options]

  --camera HOST        camera address (default 192.168.50.55)
  --nvram DIR          directory holding standalone_auth.conf, the HTTP password
                       file and the RTSP password file; enables the authenticated
                       checks and the on-camera checks
  --image FILE         padded /home image expected in flash; the first erase
                       blocks of it are compared against the camera's /dev/mtd6
  --engine-md5 MD5     expected /home/c220-media-engine hash
  --firmware-md5 MD5   expected /home/c220-firmware hash
  --network-only       skip the maintenance-shell checks
  --help               this text

Environment equivalents: C220_CAMERA, C220_NVRAM, C220_IMAGE, C220_ENGINE_MD5,
C220_FIRMWARE_MD5, C220_NETWORK_ONLY.

Exit status is 0 only when every check passed or was skipped.
EOF
}

while [ $# -gt 0 ]; do
    case $1 in
        --camera) CAMERA=$2; shift 2 ;;
        --nvram) NVRAM_DIR=$2; shift 2 ;;
        --image) IMAGE=$2; shift 2 ;;
        --engine-md5) ENGINE_MD5=$2; shift 2 ;;
        --firmware-md5) FIRMWARE_MD5=$2; shift 2 ;;
        --network-only) NETWORK_ONLY=1; shift ;;
        --help) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
done

report() {
    printf '%-4s %-9s %s\n' "$1" "$2" "$3"
}

pass() {
    pass_count=$((pass_count + 1))
    report PASS "$1" "$2"
}

fail() {
    failure_count=$((failure_count + 1))
    report FAIL "$1" "$2"
}

skip() {
    skip_count=$((skip_count + 1))
    report SKIP "$1" "$2"
}

# One line of the config, parsed rather than sourced: an operator's copy of the
# file must not be able to run commands on the machine running this script.
value_of() {
    key=$1
    file=$2
    grep -E "^[[:space:]]*${key}[[:space:]]*=" "$file" 2>/dev/null \
        | head -n 1 \
        | sed -e 's/^[^=]*=//' -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//'
}

load_credentials() {
    [ -n "$NVRAM_DIR" ] || return 0
    config="$NVRAM_DIR/standalone_auth.conf"
    if [ ! -r "$config" ]; then
        fail credentials "no readable $config"
        return 0
    fi
    HTTP_USER=$(value_of STANDALONE_HTTP_USERNAME "$config")
    RTSP_USER=$(value_of STANDALONE_RTSP_USERNAME "$config")
    [ -r "$NVRAM_DIR/standalone_http_password" ] && \
        HTTP_PASSWORD=$(cat "$NVRAM_DIR/standalone_http_password")
    [ -r "$NVRAM_DIR/standalone_rtsp_password" ] && \
        RTSP_PASSWORD=$(cat "$NVRAM_DIR/standalone_rtsp_password")
    if [ -n "$HTTP_USER" ] && [ -n "$HTTP_PASSWORD" ]; then
        pass credentials "HTTP user $HTTP_USER, RTSP user ${RTSP_USER:-none}"
    else
        fail credentials "the config does not name a usable HTTP user and password file"
    fi
}

# A TCP connect is the honest test: the ports a deployment must not reopen
# (plaintext RTSP and the maintenance shell) are checked the same way.
port_open() {
    if command -v nc >/dev/null 2>&1; then
        nc -z -w 3 "$1" "$2" >/dev/null 2>&1
    else
        timeout 3 bash -c "exec 3<>/dev/tcp/$1/$2" 2>/dev/null
    fi
}

check_health() {
    body=$(curl -skS --connect-timeout 3 --max-time 8 \
        "https://$CAMERA:8080/healthz" 2>/dev/null) || body=
    case $body in
        *'"ready":true'*) pass health "$body" ;;
        *) fail health "https://$CAMERA:8080/healthz did not report ready" ;;
    esac
}

check_ports() {
    check_one_port 322 open
    check_one_port 8080 open
    check_one_port 554 closed
}

# The maintenance shell is the last thing checked, because it is the one port the
# gate can itself open: an already-armed window is closed by the camera checks
# below, and what matters is that the camera is not left with a root shell
# listening.
check_maintenance_closed() {
    if port_open "$CAMERA" 23; then
        fail "port 23" "open; a maintenance shell is listening"
    else
        pass "port 23" "closed"
    fi
}

check_one_port() {
    port=$1
    want=$2
    if port_open "$CAMERA" "$port"; then
        have=open
    else
        have=closed
    fi
    if [ "$have" = "$want" ]; then
        pass "port $port" "$have"
    else
        fail "port $port" "expected $want, found $have"
    fi
}

check_status() {
    [ -n "$HTTP_PASSWORD" ] || { skip status "no HTTP credentials"; return 0; }
    body=$(curl -skS --digest --user "$HTTP_USER:$HTTP_PASSWORD" --max-time 10 \
        "https://$CAMERA:8080/status" 2>/dev/null) || body=
    if [ -z "$body" ]; then
        fail status "the authenticated /status request failed"
        return 0
    fi
    dropped=0
    case $body in *'"status":"streaming"'*) ;; *) dropped=1 ;; esac
    case $body in *'"enabled":true'*) ;; *) dropped=1 ;; esac
    if [ "$dropped" -eq 0 ]; then
        pass status "streaming, TLS enabled"
    else
        fail status "not streaming or TLS disabled: $body"
    fi
}

check_rtsps() {
    [ -n "$RTSP_PASSWORD" ] || { skip rtsps "no RTSP credentials"; return 0; }
    if ! command -v ffprobe >/dev/null 2>&1; then
        skip rtsps "ffprobe is not installed"
        return 0
    fi
    # ffprobe cannot verify a self-signed peer, so verification is off and the
    # probe is only used to prove the stream set.
    output=$(timeout 60 ffprobe -hide_banner -tls_verify 0 -rtsp_transport tcp \
        -timeout 15000000 -i "rtsps://$RTSP_USER:$RTSP_PASSWORD@$CAMERA:322/live" 2>&1) || true
    streams=$(printf '%s' "$output" | grep -c 'Stream #' || true)
    if printf '%s' "$output" | grep -q 'Video: hevc'; then
        pass rtsps "HEVC video with $streams stream(s) in total"
    else
        fail rtsps "no HEVC video stream: $(printf '%s' "$output" | head -n 2 | tr '\n' ' ')"
    fi
}

# Run one command block in the camera's one-boot maintenance shell and print what
# it wrote. The session is bounded, because a camera that stops answering in the
# middle of a deployment must not hang the operator.
shell_session() {
    printf '%s\nexit\n' "$1" | timeout 60 nc "$CAMERA" 23 2>/dev/null | tr -d '\r'
}

arm_maintenance_shell() {
    enable_code=$(curl -skS -o /dev/null -w '%{http_code}' --digest \
        --user "$HTTP_USER:$HTTP_PASSWORD" -X POST --max-time 10 \
        "https://$CAMERA:8080/maintenance/enable" 2>/dev/null) || enable_code=000
    # 202 is a fresh start, 200 means this boot's window is already armed; both
    # are answered before the listener is proven to exist.
    case $enable_code in 200|202) ;; *) return 1 ;; esac
    wait_for_port "$CAMERA" 23 10
}

close_maintenance_shell() {
    # Closing the window matters more than the last line of output: it is a root
    # shell on the LAN, and this boot's copy is single use anyway.
    shell_session 'killall telnetd' >/dev/null 2>&1 || true
}

wait_for_port() {
    waited=0
    while [ "$waited" -lt "$3" ]; do
        port_open "$1" "$2" && return 0
        sleep 1
        waited=$((waited + 1))
    done
    return 1
}

check_camera() {
    if [ "$NETWORK_ONLY" = 1 ]; then
        skip camera "network-only run"
        return 0
    fi
    if [ -z "$HTTP_PASSWORD" ]; then
        skip camera "no HTTP credentials, so the maintenance shell cannot be armed"
        return 0
    fi
    if ! command -v nc >/dev/null 2>&1; then
        skip camera "nc is not installed"
        return 0
    fi

    # Validated before the shell is armed, so a bad invocation cannot leave the
    # window open behind an early return.
    blocks=0
    expected_flash=
    if [ -n "$IMAGE" ]; then
        size=$(wc -c <"$IMAGE")
        if [ $((size % ERASE_BLOCK_BYTES)) -ne 0 ]; then
            fail "flash image" "$IMAGE is not a multiple of $ERASE_BLOCK_BYTES bytes"
            return 0
        fi
        blocks=$((size / ERASE_BLOCK_BYTES))
        expected_flash=$(md5sum "$IMAGE" | cut -d' ' -f1)
    fi

    if ! arm_maintenance_shell; then
        # A failed spawn clears the firmware's one-shot flag, so the usual causes
        # are a spent window (200) or a spawn error (503). Either way the checks
        # below were not performed, and saying so is the point of this script.
        fail camera "the maintenance window did not open (HTTP $enable_code); reboot and retry"
        close_maintenance_shell
        return 0
    fi

    # The camera prints marked values rather than raw command output: the shell
    # echoes what it is sent, so a value that carries its own name is the only
    # thing that can be read back unambiguously.
    commands=$(cat <<'EOF'
md5sum /home/c220-media-engine /home/c220-firmware
echo "engine_count=$(ps | grep -c '[c]220-media-engine')"
echo "media_signal_count=$(grep -c media_signal /tmp/c220-firmware.log)"
echo "amplifier=$(cat /sys/class/gpio/gpio102/value 2>/dev/null)"
EOF
)
    if [ "$blocks" -gt 0 ]; then
        commands="dd if=/dev/mtd6 bs=$ERASE_BLOCK_BYTES count=$blocks 2>/dev/null | md5sum
$commands"
    fi

    output=$(shell_session "$commands")
    close_maintenance_shell

    if [ -n "$ENGINE_MD5" ]; then
        if printf '%s' "$output" | grep -q "^$ENGINE_MD5  /home/c220-media-engine"; then
            pass "engine" "$ENGINE_MD5"
        else
            fail "engine" "expected $ENGINE_MD5, got $(printf '%s' "$output" | awk '/ \/home\/c220-media-engine$/ {print $1}')"
        fi
    fi
    if [ -n "$FIRMWARE_MD5" ]; then
        if printf '%s' "$output" | grep -q "^$FIRMWARE_MD5  /home/c220-firmware"; then
            pass "firmware" "$FIRMWARE_MD5"
        else
            fail "firmware" "expected $FIRMWARE_MD5, got $(printf '%s' "$output" | awk '/ \/home\/c220-firmware$/ {print $1}')"
        fi
    fi
    if [ -n "$expected_flash" ]; then
        if printf '%s' "$output" | grep -q "^$expected_flash  -"; then
            pass "flash" "$expected_flash over $blocks blocks"
        else
            fail "flash" "expected $expected_flash, got $(printf '%s' "$output" | awk '/  -$/ {print $1}' | head -n 1)"
        fi
    fi

    # A crashed engine is restarted by the supervisor, so the log line is what
    # distinguishes a healthy run from a deployment that only looks healthy.
    signals=$(printf '%s' "$output" | sed -n 's/^media_signal_count=//p' | tail -n 1 | tr -dc '0-9')
    case $signals in
        ''|*[!0-9]*) skip "media signal" "the log count could not be read" ;;
        0) pass "media signal" "no fatal signal in this boot" ;;
        *) fail "media signal" "$signals fatal signal(s) in this boot" ;;
    esac

    # More than one engine means the supervisor is racing a leftover process, and
    # Rockchip MPP channels cannot be shared.
    engines=$(printf '%s' "$output" | sed -n 's/^engine_count=//p' | tail -n 1 | tr -dc '0-9')
    case $engines in
        1) pass "engine count" "one media engine process" ;;
        ''|*[!0-9]*) skip "engine count" "the process count could not be read" ;;
        *) fail "engine count" "$engines media engine processes" ;;
    esac

    amplifier=$(printf '%s' "$output" | sed -n 's/^amplifier=//p' | tail -n 1 | tr -dc '01')
    case $amplifier in
        0) pass "amplifier" "GPIO 102 low" ;;
        '') skip amplifier "GPIO 102 could not be read" ;;
        *) fail amplifier "GPIO 102 is $amplifier after a deployment" ;;
    esac
}

load_credentials
check_health
check_ports
check_status
check_rtsps
check_camera
check_maintenance_closed

printf '\n%s passed, %s failed, %s skipped\n' "$pass_count" "$failure_count" "$skip_count"
[ "$failure_count" -eq 0 ]

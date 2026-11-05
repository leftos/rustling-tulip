#!/bin/sh
# A fake of macOS's `/usr/bin/security` for the claude-accounts tests, so no test touches a real Keychain.
#
# It keeps generic-password items as files under "$FAKE_SECURITY_DIR/items", one per service and account, holding the
# value as hex. It answers the three calls the crate makes, as `security` answers them:
#   find-generic-password -a <account> -w -s <service>   the value and one newline (as lowercase hex when a byte is not
#                                                        printable ASCII, as `security` prints it), or exit 44
#   add-generic-password -U -a <account> -s <service> -X <hex>   creates or replaces the item
#   delete-generic-password -a <account> -s <service>    exit 44 when there is no item
# and `-i`, which reads such command lines from stdin and re-parses each one shell-style.
#
# Each call appends its argv, joined by spaces, to "$FAKE_SECURITY_DIR/argv.log", and each `-i` line to
# "$FAKE_SECURITY_DIR/stdin.log", so a test can assert the exact calls. While "$FAKE_SECURITY_DIR/fail" exists, every
# call exits with the code it holds, as a locked or refusing Keychain does.

set -u
dir=${FAKE_SECURITY_DIR:?FAKE_SECURITY_DIR names the fake Keychain folder}
mkdir -p "$dir/items"
printf '%s\n' "$*" >> "$dir/argv.log"
if [ -f "$dir/fail" ]; then
    exit "$(cat "$dir/fail")"
fi

# The item file's name for a service and an account: the hex of "<service>\n<account>".
item_name() {
    printf '%s\n%s' "$1" "$2" | od -An -tx1 | tr -d ' \n'
}

run() {
    command=$1
    shift
    account=
    service=
    hex=
    show=
    while [ $# -gt 0 ]; do
        case $1 in
            -a) account=$2; shift 2 ;;
            -s) service=$2; shift 2 ;;
            -X) hex=$2; shift 2 ;;
            -w) show=1; shift ;;
            -U) shift ;;
            *) echo "fake security: unexpected argument $1" >&2; return 2 ;;
        esac
    done
    file="$dir/items/$(item_name "$service" "$account")"
    case $command in
        find-generic-password)
            [ -f "$file" ] || return 44
            if [ -n "$show" ]; then
                # `security -w` prints the data raw only when every byte is printable ASCII, and its hex otherwise.
                bytes=$(xxd -r -p "$file" | od -An -tu1 | tr -s '[:space:]' ' ')
                printable=1
                for byte in $bytes; do
                    if [ "$byte" -lt 32 ] || [ "$byte" -gt 126 ]; then
                        printable=0
                        break
                    fi
                done
                if [ "$printable" -eq 1 ]; then
                    xxd -r -p "$file"
                else
                    cat "$file"
                fi
                printf '\n'
            fi
            return 0 ;;
        add-generic-password)
            printf '%s' "$hex" > "$file"
            return 0 ;;
        delete-generic-password)
            [ -f "$file" ] || return 44
            rm -f "$file"
            return 0 ;;
        *)
            echo "fake security: unexpected command $command" >&2
            return 2 ;;
    esac
}

if [ "$1" = "-i" ]; then
    status=0
    while IFS= read -r line; do
        printf '%s\n' "$line" >> "$dir/stdin.log"
        eval "set -- $line"
        run "$@" || status=$?
    done
    exit "$status"
fi
run "$@"

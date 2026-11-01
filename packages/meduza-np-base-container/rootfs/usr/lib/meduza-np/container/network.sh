# Sourced by entrypoint.sh. Bootstrap networking must work before either role downloads or starts anything.
# Keep the old image's DEFAULT_GW / DEFAULT_GW_DEV and DNS_NAMESERVERS / DNS_NAMESERVER interfaces.
NETWORK_RESOLV=/etc/resolv.conf
dns_monitor_pid=""

network_error() { log network "$*"; return 1; }

network_ipv4_literal() {
	printf '%s\n' "$1" | awk -F. 'NF != 4 { exit 1 } { for (i=1; i<=4; i++) if ($i !~ /^[0-9]+$/ || $i ~ /^0[0-9]/ || $i+0 > 255) exit 1 }'
}

load_network_config() {
	network_gateway=${MEDUZA_NP_DEFAULT_GW-${DEFAULT_GW:-}}
	network_device=${MEDUZA_NP_DEFAULT_GW_DEV-${DEFAULT_GW_DEV:-}}
	network_dns=${MEDUZA_NP_DNS_NAMESERVERS-${DNS_NAMESERVERS:-${DNS_NAMESERVER:-}}}
	network_interval=${MEDUZA_NP_DNS_MONITOR_INTERVAL-${DNS_MONITOR_INTERVAL:-5}}
	if [ -n "$network_gateway" ]; then
		# Never accidentally change the host's default route or send a bridge container to a LAN-only gateway.
		[ "${MEDUZA_NP_NETWORK_MODE:-}" = macvlan ] ||
			{ network_error "DEFAULT_GW requires MEDUZA_NP_NETWORK_MODE=macvlan"; return 1; }
		network_ipv4_literal "$network_gateway" ||
			{ network_error "DEFAULT_GW must be an IPv4 address"; return 1; }
		ip -4 route show match "$network_gateway/32" >/dev/null 2>&1 ||
			{ network_error "invalid DEFAULT_GW: $network_gateway"; return 1; }
	fi
	case "$network_device" in *[!a-zA-Z0-9_.:-]*) network_error "invalid DEFAULT_GW_DEV"; return 1 ;; esac
	[ -z "$network_device" ] || [ -n "$network_gateway" ] ||
		{ network_error "DEFAULT_GW_DEV requires DEFAULT_GW"; return 1; }
	[ -n "$network_dns" ] || return 0
	case "$network_interval" in '' | *[!0-9]*) network_error "DNS_MONITOR_INTERVAL must be a positive integer"; return 1 ;; esac
	[ "$network_interval" -gt 0 ] 2>/dev/null ||
		{ network_error "DNS_MONITOR_INTERVAL must be a positive integer"; return 1; }
	# Check characters before word splitting, so shell patterns and resolver hostnames cannot be expanded/resolved.
	case "$network_dns" in *[!0-9a-fA-F:.,[:space:]]*) network_error "DNS_NAMESERVERS must contain IP addresses"; return 1 ;; esac
	network_dns=$(printf '%s' "$network_dns" | tr ',\t\r\n' '    ')
	network_nameservers=""
	for ns in $network_dns; do
		case "$ns" in
		*:*) family=6; prefix=128 ;;
		*)
			family=4; prefix=32
			network_ipv4_literal "$ns" || { network_error "invalid DNS server: $ns"; return 1; }
			;;
		esac
		ip -"$family" route show match "$ns/$prefix" >/dev/null 2>&1 ||
			{ network_error "invalid DNS server: $ns"; return 1; }
		network_nameservers="${network_nameservers}${ns}
"
	done
	[ -n "$network_nameservers" ] || { network_error "DNS_NAMESERVERS is empty"; return 1; }
}

apply_gateway() {
	[ -n "$network_gateway" ] || return 0
	set -- ip -4 route replace default via "$network_gateway"
	[ -z "$network_device" ] || set -- "$@" dev "$network_device"
	"$@" || { network_error "cannot set default gateway to $network_gateway"; return 1; }
	log network "default gateway: $network_gateway ${network_device:+dev $network_device}"
}

apply_dns() {
	[ -n "$network_dns" ] || return 0
	actual=$(awk '$1 == "nameserver" { print $2 }' "$NETWORK_RESOLV" 2>/dev/null || true)
	[ "$actual" != "${network_nameservers%?}" ] || return 0
	# Preserve search/options, and write through the existing file: runtimes bind-mount resolv.conf (no rename).
	resolv_tmp=$(mktemp) || { network_error "cannot prepare system DNS file"; return 1; }
	if [ -e "$NETWORK_RESOLV" ] && ! awk '$1 != "nameserver" { print }' "$NETWORK_RESOLV" >"$resolv_tmp"; then
		rm -f "$resolv_tmp"
		network_error "cannot read $NETWORK_RESOLV"
		return 1
	fi
	if ! printf '%s' "$network_nameservers" | while IFS= read -r ns; do printf 'nameserver %s\n' "$ns"; done >>"$resolv_tmp"; then
		rm -f "$resolv_tmp"
		network_error "cannot prepare system DNS file"
		return 1
	fi
	if ! cat "$resolv_tmp" >"$NETWORK_RESOLV"; then
		rm -f "$resolv_tmp"
		network_error "cannot write $NETWORK_RESOLV"
		return 1
	fi
	rm -f "$resolv_tmp"
	log network "system DNS: $network_dns"
}

dns_monitor_loop() {
	dns_sleep_pid=""
	trap '[ -z "$dns_sleep_pid" ] || kill "$dns_sleep_pid" 2>/dev/null || true; exit 0' TERM INT
	while :; do
		sleep "$network_interval" &
		dns_sleep_pid=$!
		wait "$dns_sleep_pid" || return 0
		dns_sleep_pid=""
		apply_dns || log network "DNS restore failed; retrying in ${network_interval}s"
	done
}

stop_dns_monitor() {
	[ -n "$dns_monitor_pid" ] || return 0
	kill -TERM "$dns_monitor_pid" 2>/dev/null || true
	wait "$dns_monitor_pid" 2>/dev/null || true
	dns_monitor_pid=""
}

start_network() {
	load_network_config
	apply_gateway
	apply_dns
	if [ -n "$network_dns" ]; then
		dns_monitor_loop &
		dns_monitor_pid=$!
	fi
}

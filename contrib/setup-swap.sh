#!/usr/bin/env bash
# Ridimensiona /swapfile (ext4) e abbassa vm.swappiness. Da eseguire con sudo.
#   sudo ./setup-swap.sh [dimensione_GB]     (default 8)
set -euo pipefail

SIZE_GB="${1:-8}"
SWAPFILE=/swapfile
SYSCTL_CONF=/etc/sysctl.d/99-memguard-swap.conf

[ "$(id -u)" -eq 0 ] || { echo "Esegui con sudo." >&2; exit 1; }
[ "$(findmnt -no FSTYPE -T "$(dirname "$SWAPFILE")")" = ext4 ] || { echo "Il filesystem non è ext4: procedura non supportata." >&2; exit 1; }

need_kb=$(( SIZE_GB * 1024 * 1024 ))
free_kb=$(df --output=avail -k "$(dirname "$SWAPFILE")" | tail -1)
[ "$free_kb" -gt $(( need_kb + 20 * 1024 * 1024 )) ] || { echo "Spazio insufficiente (servono ${SIZE_GB} GB + 20 GB di margine)." >&2; exit 1; }

# Prima di disattivare lo swapfile, verifica che la RAM possa assorbire le sue pagine.
if swapon --noheadings --show=NAME | grep -qx "$SWAPFILE"; then
    used_kb=$(awk -v f="$SWAPFILE" '$1==f {print $4}' /proc/swaps)
    avail_kb=$(awk '/MemAvailable/ {print $2}' /proc/meminfo)
    [ "$avail_kb" -gt $(( used_kb * 2 )) ] || { echo "RAM disponibile insufficiente per svuotare lo swapfile." >&2; exit 1; }
    echo "Disattivo $SWAPFILE (le pagine tornano in RAM)..."
    swapoff "$SWAPFILE"
fi

echo "Creo $SWAPFILE da ${SIZE_GB} GB..."
rm -f "$SWAPFILE"
fallocate -l "${SIZE_GB}G" "$SWAPFILE"
chmod 600 "$SWAPFILE"
mkswap "$SWAPFILE" >/dev/null
swapon "$SWAPFILE"

grep -qE "^$SWAPFILE[[:space:]]" /etc/fstab || echo "$SWAPFILE none swap defaults 0 0" >> /etc/fstab

cat > "$SYSCTL_CONF" <<EOF
# Con molta RAM il kernel deve preferire la cache all'uso dello swap.
vm.swappiness = 20
EOF
sysctl -q -p "$SYSCTL_CONF"

echo
swapon --show
echo "vm.swappiness = $(cat /proc/sys/vm/swappiness)"

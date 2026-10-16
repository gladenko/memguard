# memguard

Monitor della memoria per **KDE Plasma** e **GNOME**, scritto in Rust. Non richiede root.

- **Monitora** RAM (`MemAvailable`), swap e la pressione PSI (`/proc/pressure/memory`), il segnale più affidabile di un rallentamento imminente.
- **Avvisa** con una notifica desktop (`org.freedesktop.Notifications`) quando la memoria sta per saturarsi.
- **Libera** memoria quando il rallentamento è imminente, scrivendo in `memory.reclaim` del cgroup della sessione utente (il kernel scarta cache e sposta pagine fredde in swap).
- **Icona nella barra** (StatusNotifierItem): verde = ok, giallo = ci si sta avvicinando alla soglia, rosso = soglia critica. Tooltip con RAM/swap/pressione; menu con "Libera memoria ora" ed "Esci". Su GNOME serve l'estensione *AppIndicator and KStatusNotifierItem Support*; con `--no-tray` si disattiva.
- Opzionale `--root-clean`: svuota anche la cache di sistema via `pkexec`.

## Installazione

Da sorgente:

    cargo build --release
    install -Dm755 target/release/memguard ~/.local/bin/memguard
    install -Dm644 contrib/memguard.service ~/.config/systemd/user/memguard.service
    systemctl --user enable --now memguard

Da pacchetto (release su GitHub: `.deb` per Debian/Ubuntu, `.rpm` per Fedora/openSUSE):

    sudo dpkg -i memguard_0.1.0-1_amd64.deb     # Debian/Ubuntu
    sudo rpm -i memguard-0.1.0-1.x86_64.rpm   # Fedora/openSUSE

    systemctl --user enable --now memguard    # l'unità è in /usr/lib/systemd/user/

## Uso

    memguard --once                 # stato attuale
    memguard --dry-run              # notifica senza liberare nulla
    memguard --warn 80 --critical 90 --interval 3

Livelli (colore icona): **giallo** da 90% della soglia di avviso; **avviso** (notifica) se RAM ≥ 85% o PSI ≥ 10; **critico** (pulizia) se RAM ≥ 93%, PSI ≥ 25, oppure RAM ≥ 85% con swap ≥ 60%.
Dopo ogni intervento c'è una pausa (`--cooldown`, 120 s) per non inondare di notifiche.

Requisiti: kernel con cgroup v2 e PSI, `notify-send` (libnotify), systemd.

## Script di supporto

`contrib/setup-swap.sh [GB]` ridimensiona `/swapfile` (ext4) e imposta `vm.swappiness = 20`; va eseguito con `sudo`.

## Licenza

GPL-3.0-or-later, vedi `LICENSE`.

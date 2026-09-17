#!/bin/sh
# Eufy C220 (T8W11) MicroSD Autorun Root Script
# Executed automatically by /home/script/app.sh at camera boot

LOG="/mnt/sdcard/iq_boot.log"

echo "=== IQ.SH EXECUTION STARTED ===" > "$LOG"
echo "Kernel: $(uname -a)" >> "$LOG"
echo "Uptime: $(uptime)" >> "$LOG"

# 1. Start Root Telnet Server on Port 23
echo "[*] Starting root telnetd on port 23..." >> "$LOG"
telnetd -p 23 -l /bin/sh &

# 2. Dump Network Interfaces and IP
echo "[*] Network configuration:" >> "$LOG"
ifconfig >> "$LOG"

# 3. Dump Running Processes
echo "[*] Process list:" >> "$LOG"
ps -ef >> "$LOG" 2>&1 || ps >> "$LOG"

# 4. Dump Flash Partition Tables & MTD info
echo "[*] MTD Partitions:" >> "$LOG"
cat /proc/mtd >> "$LOG" 2>&1

# 5. Backup NVRAM Config to SD Card if not already backed up
if [ ! -d "/mnt/sdcard/eufy_nvram_backup" ]; then
    echo "[*] Backing up /home/nvram to SD card..." >> "$LOG"
    mkdir -p /mnt/sdcard/eufy_nvram_backup
    cp -rf /home/nvram/* /mnt/sdcard/eufy_nvram_backup/ 2>/dev/null
    cp -rf /home/config/* /mnt/sdcard/eufy_nvram_backup/ 2>/dev/null
fi

# 6. Execute optional user custom script if present
if [ -f "/mnt/sdcard/custom.sh" ]; then
    echo "[*] Executing /mnt/sdcard/custom.sh..." >> "$LOG"
    chmod +x /mnt/sdcard/custom.sh
    /mnt/sdcard/custom.sh >> "$LOG" 2>&1 &
fi

echo "=== IQ.SH INITIALIZATION COMPLETE ===" >> "$LOG"
sync

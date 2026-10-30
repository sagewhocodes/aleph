//! Live machine stats for the dashboard and the Monitor view: CPU (total, per
//! core, user/system/iowait/steal), memory breakdown, swap, load, temperatures,
//! GPUs (nvidia-smi / rocm-smi), GPU processes, the process table, per-interface
//! network and per-device disk throughput, disks, agent processes and a few
//! session facts — streamed from one `ssh <host> sh -s` loop while a view needs
//! them, and killed the moment it doesn't. Nothing is installed remotely.
//!
//! The remote loop ([`script`]) prints tagged, tab-separated lines and an `E`
//! line per sample; rates (CPU %, per-process CPU, network, disk) are computed
//! here from counter deltas, like htop does. Every tool the script uses may be
//! missing or fail (BusyBox, containers, old drivers): it then just prints less,
//! and [`Acc`] fills whatever it got.

use crate::agents;
use crate::sshcfg;
use crate::ui::{self, c};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Samples kept per history (5 minutes at the default 1 s interval).
const HIST: usize = 300;
/// Rows kept in [`Stats::procs`] (the busiest, plus their ancestors so the
/// process tree stays whole).
const PROC_ROWS: usize = 400;
/// Rows kept in [`Stats::top`].
const TOP_ROWS: usize = 8;

/// The remote sampler: POSIX sh + awk (dash, mawk, BusyBox, macOS). Sent on
/// stdin, so it can be long and commented. `@I@` is the interval in seconds.
///
/// Tags (one sample = everything up to `E`):
///
/// | tag | fields | when |
/// |---|---|---|
/// | `H` | hostname, `uname -sr`, OS name | once |
/// | `C` | ncpu, CPU model, CLK_TCK, page size | once |
/// | `S` | a `/proc/stat` `cpu…` line | Linux |
/// | `M` | `Key:`, kB (meminfo names; macOS maps vm_stat / swapusage onto them) | |
/// | `L` | `l1 l5 l15`, `running/threads` | |
/// | `U` | uptime seconds | |
/// | `N` | interface, rx bytes, tx bytes (counters) | |
/// | `B` | device, sectors read, sectors written, io ms (Linux) | |
/// | `Y` | hwmon dir, chip, sensor, label, m°C (`zone` chip = thermal zone) | Linux, every 2nd |
/// | `F` | average MHz, fastest core MHz, rated max kHz (cpufreq) | Linux, every 2nd |
/// | `Q` | pid, `utime+stime` ticks, rss pages, comm, state, threads, ppid, start ticks | Linux |
/// | `T` | pid, user, %cpu, rss kB, comm, state, cputime, ppid, elapsed | macOS |
/// | `J` | pid, threads | macOS, every 3rd |
/// | `W` | pid, user, args (≤200 chars) | every 3rd |
/// | `c` | us, sy, id (% over the last pause) | macOS |
/// | `G` | nvidia-smi CSV (basic 10 or extended 22 fields) | |
/// | `R` | a rocm-smi CSV line | every 3rd |
/// | `g` | a GPU tool ran (`G`/`R` lines are the whole GPU list) | |
/// | `A` | bus, pid, MiB, user, name (nvidia compute app) | every 3rd |
/// | `V` | pid, folder (a compute app's working directory, Linux) | every 3rd |
/// | `a` | the `A` list is complete | every 3rd |
/// | `P` | an `nvidia-smi pmon -s um` line (headers too) | every 3rd |
/// | `p` | the `P` list is complete | every 3rd |
/// | `Z` | model, cores, device %, renderer %, tiler %, memory in use, allocated, the Mac's memory (bytes) — an Apple GPU from the I/O Registry (no sudo) | macOS |
/// | `X` | `none`/`nvidia`/`amd`, message: why there are no GPU stats | |
/// | `D` | mount, total kB, used kB | every 5th |
/// | `O` | logged-in users | every 10th |
/// | `K` | count, agent: session logs touched in 24h | every 15th |
/// | `E` | epoch seconds — end of sample | |
const SCRIPT: &str = r#"# aleph machine stats sampler. POSIX sh + awk; tools that may be
# missing are probed once, and every command keeps its errors to itself so the
# stream stays clean and the loop never stops.
export LC_ALL=C
I=@I@
n=0
linux=0; [ -r /proc/stat ] && linux=1
mac=0; [ "$(uname -s 2>/dev/null)" = Darwin ] && mac=1
TO=; TP=; timeout 5 true >/dev/null 2>&1 && TO="timeout 8" && TP="timeout 3"
IOS=; [ $mac = 1 ] && command -v iostat >/dev/null 2>&1 && IOS=1
WHO=; command -v who >/dev/null 2>&1 && WHO=1

# ---- once: host, OS, CPU model, clock ticks, page size
os=$( (. /etc/os-release && echo "$PRETTY_NAME") 2>/dev/null)
[ -n "$os" ] || { os=$(sw_vers -productName 2>/dev/null) && os="$os $(sw_vers -productVersion 2>/dev/null)"; }
[ -n "$os" ] || os=$(uname -s)
printf 'H\t%s\t%s\t%s\n' "$(uname -n)" "$(uname -sr)" "$os"
nc=$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null)
cm=$(awk -F': *' '/^model name/ { print $2; exit }' /proc/cpuinfo 2>/dev/null)
[ -n "$cm" ] || cm=$(sysctl -n machdep.cpu.brand_string 2>/dev/null)
[ -n "$cm" ] || cm=$(lscpu 2>/dev/null | awk -F': *' '/^Model name/ { print $2; exit }')
[ -n "$cm" ] || cm=$(awk -F': *' '/^(Model|Hardware)[ \t]*:/ { print $2; exit }' /proc/cpuinfo 2>/dev/null)
ck=$(getconf CLK_TCK 2>/dev/null)
pz=$(getconf PAGESIZE 2>/dev/null || getconf PAGE_SIZE 2>/dev/null)
sk=$(awk -F': *' '/^physical id/ { s[$2] = 1 } END { for (k in s) n++; if (n) print n }' /proc/cpuinfo 2>/dev/null)
[ -n "$sk" ] || sk=$(sysctl -n hw.packages 2>/dev/null)
printf 'C\t%s\t%s\t%s\t%s\t%s\n' "$nc" "$cm" "$ck" "$pz" "$sk"

# ---- Linux: one awk over the /proc files that exist (mawk aborts on a missing one)
PF=
for f in /proc/stat /proc/meminfo /proc/loadavg /proc/uptime /proc/net/dev /proc/diskstats; do
  [ -r "$f" ] && PF="$PF $f"
done
sys_linux() { awk '@SYS_AWK@' $PF 2>/dev/null; }
# cat skips processes that exit meanwhile (awk would stop at the first missing file)
procs_linux() { cat /proc/[0-9]*/stat 2>/dev/null | awk '@STAT_AWK@'; }
# hwmon (name, temp*_label, temp*_input; old kernels keep them under device/)
# and thermal zones, read by one grep (it shrugs off unreadable sensors).
# the CPU clock: /proc/cpuinfo's per-core MHz, and the rated maximum once
FMAX=$(awk 'm + 0 < $1 + 0 { m = $1 } END { if (m) print m }' /sys/devices/system/cpu/cpu*/cpufreq/cpuinfo_max_freq 2>/dev/null)
freq_linux() {
  awk -v mx="$FMAX" -F': *' '/^cpu MHz/ { v = $2 + 0; s += v; k++; if (v > m) m = v }
    END { if (k) printf "F\t%.0f\t%.0f\t%s\n", s / k, m, mx }' /proc/cpuinfo 2>/dev/null
}
temps_linux() {
  hw=/sys/class/hwmon/hwmon*
  grep -s . /dev/null $hw/name $hw/temp*_label $hw/temp*_input $hw/device/name $hw/device/temp*_label $hw/device/temp*_input \
    /sys/class/thermal/thermal_zone*/type /sys/class/thermal/thermal_zone*/temp 2>/dev/null | awk '@TEMP_AWK@'
}

# ---- macOS: sysctl, vm_stat, netstat, ioreg, ps (no sudo needed for any)
sys_mac() {
  printf 'L\t%s\t\n' "$(sysctl -n vm.loadavg 2>/dev/null | tr -d '{}')"
  sysctl -n hw.memsize 2>/dev/null | awk '$1 > 0 { printf "M\tMemTotal:\t%.0f\n", $1 / 1024 }'
  vm_stat 2>/dev/null | awk '
    /page size of/ { for (i = 1; i < NF; i++) if ($i == "of") ps = $(i + 1) }
    { v = $NF; sub(/\.$/, "", v) }
    /^Pages free:/ { fr = v }
    /^Pages inactive:/ { ia = v }
    /^Pages speculative:/ { sp = v }
    /^File-backed pages:/ { fb = v }
    END {
      if (ps + 0 <= 0) ps = 4096
      k = ps / 1024
      if (fr != "") printf "M\tMemAvailable:\t%.0f\nM\tMemFree:\t%.0f\nM\tCached:\t%.0f\n", (fr + ia + sp) * k, fr * k, fb * k
    }'
  sysctl -n vm.swapusage 2>/dev/null | awk '
    function kb(x,   u) { u = substr(x, length(x)); x += 0; if (u == "G") return x * 1048576; if (u == "M") return x * 1024; if (u == "K") return x; return x / 1024 }
    { for (i = 1; i < NF; i++) { if ($i == "total") t = kb($(i + 2)); if ($i == "free") f = kb($(i + 2)) } }
    END { if (t != "") printf "M\tSwapTotal:\t%.0f\nM\tSwapFree:\t%.0f\n", t, f }'
  sysctl -n kern.boottime 2>/dev/null | awk -v now="$(date +%s)" '
    { for (i = 1; i < NF; i++) if ($i == "sec") { s = $(i + 2); sub(/,/, "", s); if (s + 0 > 0) printf "U\t%d\n", now - s } }'
  # one <Link#N> row per interface carries its counters (the address rows repeat them)
  netstat -ibn 2>/dev/null | awk '$3 ~ /^<Link#/ { nm = $1; sub(/\*$/, "", nm); print "N\t" nm "\t" $(NF - 4) "\t" $(NF - 1) }'
  ioreg -c IOBlockStorageDriver -r -w 0 -d 2 -l 2>/dev/null | awk '
    function val(s, key,   i, t) { i = index(s, key); if (!i) return ""; t = substr(s, i + length(key)); sub(/[^0-9].*$/, "", t); return t }
    /"Statistics" = / { r = val($0, "\"Bytes (Read)\"="); w = val($0, "\"Bytes (Write)\"="); have = 1; next }
    /"BSD Name" = / && have { nm = $0; sub(/.*"BSD Name" = "/, "", nm); sub(/".*/, "", nm); printf "B\t%s\t%.0f\t%.0f\t\n", nm, r / 512, w / 512; have = 0 }'
}
procs_mac() {
  ps -A -o pid=,ppid=,user=,pcpu=,rss=,state=,time=,etime=,comm= 2>/dev/null | awk '$1 ~ /^[0-9]+$/ {
    c = $9; for (i = 10; i <= NF; i++) c = c " " $i
    print "T\t" $1 "\t" $3 "\t" $4 "\t" $5 "\t" c "\t" $6 "\t" $7 "\t" $2 "\t" $8
  }'
}
# ps -M: one row per thread; a process's first row starts with its user
threads_mac() {
  ps -AM 2>/dev/null | awk 'NR > 1 { p = ($0 ~ /^[^ ]/) ? $2 : $1; t[p]++ } END { for (p in t) print "J\t" p "\t" t[p] }'
}

# ---- users and command lines (procps; BusyBox ps has no -A/user:N but keeps going)
psargs() {
  {
    if [ $linux = 1 ]; then
      ps -A -o pid=,user:32=,args= 2>/dev/null || ps -A -o pid=,user=,args= 2>/dev/null || ps -o pid=,user=,args= 2>/dev/null
    else
      ps -A -o pid=,user=,args= 2>/dev/null
    fi
  } | awk '$1 ~ /^[0-9]+$/ { p = $1; u = $2; sub(/^[ \t]*[0-9]+[ \t]+[^ \t]+[ \t]*/, ""); gsub(/\t/, " "); print "W\t" p "\t" u "\t" substr($0, 1, 200) }'
}

# ---- GPUs. nvidia-smi fails the WHOLE query when it doesn't know one field
# (older drivers), so try the richest field list first and remember the first
# that works (gq); when none does, wait a while before probing again (gw).
NV=$(command -v nvidia-smi 2>/dev/null)
for p in /usr/lib/wsl/lib/nvidia-smi /usr/local/nvidia/bin/nvidia-smi; do
  [ -z "$NV" ] && [ -x "$p" ] && NV=$p
done
RS=$(command -v rocm-smi 2>/dev/null)
[ -z "$RS" ] && [ -x /opt/rocm/bin/rocm-smi ] && RS=/opt/rocm/bin/rocm-smi
GB=index,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw,power.limit,fan.speed,pci.bus_id
GX=$GB,utilization.memory,clocks.sm,clocks.mem,clocks.max.sm,pstate,pcie.link.gen.current,pcie.link.width.current,utilization.encoder,utilization.decoder,temperature.memory
GE=ecc.errors.uncorrected.volatile.total
gq=; gw=0; gm=
nvq() { $TO "$NV" --query-gpu="$1" --format=csv,noheader,nounits 2>&1; }
first() { printf '%s\n' "$1" | awk 'NF && !/^=+/ { gsub(/\t/, " "); print; exit }'; }
gpu_nv() {
  if [ -z "$gq" ]; then
    if [ "$n" -lt "$gw" ]; then printf 'X\tnvidia\t%s\ng\n' "$gm"; return; fi
    for q in "$GX,clocks_event_reasons.active,$GE" "$GX,clocks_throttle_reasons.active,$GE" "$GB"; do
      if o=$(nvq "$q"); then gq=$q; break; fi
      gm=$(first "$o")
    done
    if [ -z "$gq" ]; then
      gw=$((n + 30))
      # no working NVIDIA driver, but AMD tools: use those from now on
      if [ -n "$RS" ]; then NV=; return; fi
      printf 'X\tnvidia\t%s\ng\n' "$gm"; return
    fi
  elif o=$(nvq "$gq"); then :
  else
    gq=; gw=$((n + 5)); gm=$(first "$o")
    printf 'X\tnvidia\t%s\ng\n' "$gm"; return
  fi
  printf '%s\n' "$o" | awk 'NF { print "G\t" $0 }'
  echo g
  if [ $((n % 3)) -eq 0 ]; then
    gapps=$($TO "$NV" --query-compute-apps=gpu_bus_id,pid,used_memory,process_name --format=csv,noheader,nounits 2>/dev/null)
    printf '%s\n' "$gapps" | awk -F', *' '$2 ~ /^[0-9]+$/ { nm = $4; for (i = 5; i <= NF; i++) nm = nm "," $i; print "A\t" $1 "\t" $2 "\t" $3 "\t\t" nm }'
    # where each one runs: its folder (which session it's from)
    for gpid in $(printf '%s\n' "$gapps" | awk -F', *' '$2 ~ /^[0-9]+$/ { print $2 }'); do
      gdir=$(readlink "/proc/$gpid/cwd" 2>/dev/null) && printf 'V\t%s\t%s\n' "$gpid" "$gdir"
    done
    echo a
    # per-process SM / memory-bus / codec use (graphics processes too); old drivers just print nothing
    $TP "$NV" pmon -c 1 -s um 2>/dev/null | awk 'NF { gsub(/\t/, " "); print "P\t" $0 }'
    echo p
  fi
}
# rocm-smi (python, slow): every 3rd sample; its CSV varies between versions,
# so it goes out raw and is read by header names. Exit codes vary too: success
# is a "device," header.
RX="--showuse --showmemuse --showmeminfo vram --showtemp --showpower --showmaxpower --showproductname --showbus --showfan --showclocks --csv"
RB="--showuse --showmemuse --showmeminfo vram --showtemp --showpower --csv"
rq=
gpu_amd() {
  o=
  if [ "$rq" != b ]; then
    o=$($TO "$RS" $RX 2>&1)
    case $o in *device,*) rq=x ;; *) o= ;; esac
  fi
  if [ -z "$o" ]; then
    o=$($TO "$RS" $RB 2>&1)
    case $o in *device,*) rq=b ;; esac
  fi
  case $o in
    *device,*) printf '%s\n' "$o" | awk 'index($0, ",") && !/^(WARNING|ERROR)/ { gsub(/\t/, " "); print "R\t" $0 }' ;;
    *) printf 'X\tamd\t%s\n' "$(first "$o")" ;;
  esac
  echo g
}
# a Mac's GPUs (Apple silicon's, or an Intel Mac's): their own statistics in
# the I/O Registry, no sudo — utilisation (device, renderer, tiler), memory in
# use and allocated (of the memory it shares), the model and its cores. No
# power, temperature or clock: those need powermetrics (sudo).
MSZ=; [ $mac = 1 ] && MSZ=$(sysctl -n hw.memsize 2>/dev/null)
gpu_apple() {
  o=$($TP ioreg -r -d 1 -w 0 -c IOAccelerator 2>/dev/null | awk -v msz="$MSZ" '
    function val(s, key,   i, r) { i = index(s, "\"" key "\"="); if (!i) return ""; r = substr(s, i + length(key) + 3); sub(/[,}].*/, "", r); return r }
    function flush() { if (seen) printf "Z\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n", model, cores, dev, ren, til, inuse, alloc, msz; seen = 0; model = ""; cores = "" }
    /^\+-o/ { flush() }
    /"model" = / { m = $0; sub(/.*"model" = <?"/, "", m); sub(/".*/, "", m); model = m }
    /"gpu-core-count" = / { c = $0; sub(/.*= /, "", c); cores = c }
    /"PerformanceStatistics" = / { s = $0; seen = 1; dev = val(s, "Device Utilization %"); ren = val(s, "Renderer Utilization %"); til = val(s, "Tiler Utilization %"); inuse = val(s, "In use system memory"); alloc = val(s, "Alloc system memory") }
    END { flush() }')
  if [ -n "$o" ]; then printf '%s\n' "$o"; echo g
  else printf 'X\tnone\t\n'
  fi
}
gpu() {
  if [ -n "$NV" ]; then gpu_nv
  elif [ -n "$RS" ]; then [ $((n % 3)) -eq 0 ] && gpu_amd
  elif [ $mac = 1 ]; then gpu_apple
  else printf 'X\tnone\t\n'
  fi
}

sessions() {
  [ -d "$HOME/.claude/projects" ] && printf 'K\t%s\tclaude\n' "$(find "$HOME/.claude/projects" -maxdepth 2 -name '*.jsonl' -mmin -1440 2>/dev/null | wc -l | tr -d ' ')"
  [ -d "$HOME/.codex/sessions" ] && printf 'K\t%s\tcodex\n' "$(find "$HOME/.codex/sessions" -name '*.jsonl' -mmin -1440 2>/dev/null | wc -l | tr -d ' ')"
}

# macOS has no CPU tick counters for sh: wait with iostat instead of sleep, and
# report the CPU split over that wait (it lands in the next sample).
pause() {
  if [ -n "$IOS" ] && [ "$I" -ge 1 ]; then
    o=$(iostat -d -C -n 0 -c 2 -w "$I" 2>/dev/null | awk '$1 ~ /^[0-9]/ && NF >= 3 { u = $1; s = $2; d = $3 } END { if (u != "") print "c\t" u "\t" s "\t" d }')
    if [ -n "$o" ]; then printf '%s\n' "$o"; return; fi
  fi
  sleep "$I"
}

while :; do
  if [ $linux = 1 ]; then
    sys_linux
    procs_linux
    [ $((n % 2)) -eq 0 ] && { temps_linux; freq_linux; }
  else
    sys_mac
    procs_mac
    [ $((n % 3)) -eq 0 ] && threads_mac
  fi
  [ $((n % 3)) -eq 0 ] && psargs
  gpu
  [ $((n % 5)) -eq 0 ] && df -Pk / "$HOME" 2>/dev/null | awk 'NR > 1 { print "D\t" $6 "\t" $2 "\t" $3 }'
  [ $((n % 10)) -eq 0 ] && [ -n "$WHO" ] && printf 'O\t%s\n' "$(who 2>/dev/null | wc -l | tr -d ' ')"
  [ $((n % 15)) -eq 0 ] && sessions
  printf 'E\t%s\n' "$(date +%s)"
  n=$((n + 1))
  pause
done
"#;

/// Linux system counters from `/proc/{stat,meminfo,loadavg,uptime,net/dev,diskstats}`
/// in one pass, dispatched on the file's base name (so tests can feed fake files).
const SYS_AWK: &str = r#"
  FNR == 1 { f = FILENAME; sub(/.*\//, "", f) }
  f == "stat" { if ($1 ~ /^cpu/) { s = "S\t" $1; for (i = 2; i <= NF; i++) s = s " " $i; print s }; next }
  f == "meminfo" {
    if ($1 ~ /^(MemTotal|MemFree|MemAvailable|Buffers|Cached|SReclaimable|Shmem|SwapTotal|SwapFree):$/) print "M\t" $1 "\t" $2
    next
  }
  f == "loadavg" { print "L\t" $1 " " $2 " " $3 "\t" $4; next }
  f == "uptime" { u = $1; sub(/\..*/, "", u); print "U\t" u; next }
  f == "dev" {
    # long names have no padding and big counters touch the colon: split at it
    i = index($0, ":")
    if (FNR > 2 && i) { nm = substr($0, 1, i - 1); gsub(/[ \t]/, "", nm); split(substr($0, i + 1), v, " "); print "N\t" nm "\t" v[1] "\t" v[9] }
    next
  }
  f == "diskstats" {
    # whole devices only (dm-N too: used when there is nothing else)
    if ($3 ~ /^(sd[a-z]+|px[a-z]+|xvd[a-z]+|hd[a-z]+|nvme[0-9]+n[0-9]+|md[0-9]+|mmcblk[0-9]+|dm-[0-9]+)$/) print "B\t" $3 "\t" $6 "\t" $10 "\t" $13
    next
  }
"#;

/// `/proc/PID/stat` lines → `Q`. The name sits in parentheses and may hold
/// spaces and parens, so the fields are whatever follows the LAST `) `.
/// utime/stime go out as `u+s`: old mawk prints big sums in exponent form.
const STAT_AWK: &str = r#"{
  o = index($0, " ("); k = split($0, p, /[)] /)
  if (!o || k < 2) next
  r = p[k]; nm = substr($0, o + 2, length($0) - length(r) - o - 3); gsub(/\t/, " ", nm)
  split(r, f, " ")
  print "Q\t" substr($0, 1, o - 1) "\t" f[12] "+" f[13] "\t" f[22] "\t" nm "\t" f[1] "\t" f[18] "\t" f[2] "\t" f[20]
}"#;

/// `grep . files…` output (`path:value`) from hwmon and thermal zones → `Y`.
const TEMP_AWK: &str = r#"
  {
    i = index($0, ":"); if (!i) next
    p = substr($0, 1, i - 1); v = substr($0, i + 1); gsub(/\t/, " ", v); sub(/\/device\//, "/", p)
    k = split(p, a, "/"); d = a[k - 1]; f = a[k]
    if (d ~ /^thermal_zone/) { if (f == "type") zt[d] = v; else zv[d] = v; next }
    if (f == "name") { nm[d] = v; next }
    s = f; sub(/_(input|label)$/, "", s)
    if (f ~ /_input$/) tv[d "/" s] = v; else tl[d "/" s] = v
  }
  END {
    for (x in tv) { split(x, a, "/"); print "Y\t" a[1] "\t" nm[a[1]] "\t" a[2] "\t" tl[x] "\t" tv[x] }
    for (d in zv) print "Y\t" d "\tzone\ttemp\t" zt[d] "\t" zv[d]
  }
"#;

/// The sampler for a given interval (seconds).
pub fn script(interval: u64) -> String {
    SCRIPT
        .replace("@SYS_AWK@", SYS_AWK)
        .replace("@STAT_AWK@", STAT_AWK)
        .replace("@TEMP_AWK@", TEMP_AWK)
        .replace("@I@", &interval.to_string())
}

#[derive(Clone, Debug, Default)]
pub struct Gpu {
    pub index: String,
    pub name: String,
    pub util: f64,
    pub mem_used: f64,
    pub mem_total: f64,
    pub temp: Option<f64>,
    pub power: Option<f64>,
    pub power_limit: Option<f64>,
    pub fan: Option<f64>,
    pub bus: String,
    /// utilisation history (0..1), one point per sample
    pub hist: Vec<f64>,
    /// `nvidia` | `amd`
    pub vendor: String,
    /// memory-controller utilisation (0..1)
    pub mem_util: Option<f64>,
    /// MHz
    pub clock_sm: Option<f64>,
    pub clock_mem: Option<f64>,
    pub clock_sm_max: Option<f64>,
    /// `P0`…`P12`
    pub pstate: String,
    /// `gen4 x16`
    pub pcie: String,
    /// encoder / decoder utilisation (0..1)
    pub enc: Option<f64>,
    pub dec: Option<f64>,
    /// memory temperature (°C, HBM cards)
    pub temp_mem: Option<f64>,
    /// active clock-throttle reasons, human words (`power cap`, `thermal`, …)
    pub throttle: Vec<String>,
    /// uncorrected ECC errors since boot
    pub ecc: Option<u64>,
    /// an Apple GPU: its renderer and tiler utilisation (0..1), its cores,
    /// and the memory allocated for it (bytes; `mem_used` is what's in use
    /// of the Mac's memory, which it shares)
    pub renderer: Option<f64>,
    pub tiler: Option<f64>,
    pub cores: Option<u32>,
    pub mem_alloc: Option<f64>,
    /// VRAM use history (0..1)
    pub mem_hist: Vec<f64>,
    /// power draw history (fraction of the limit, 0..1)
    pub power_hist: Vec<f64>,
    /// core temperature history (°C)
    pub temp_hist: Vec<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct GpuProc {
    pub gpu: String,
    pub pid: String,
    /// VRAM in use, bytes
    pub mem: f64,
    pub user: String,
    pub name: String,
    /// `C` compute · `G` graphics · `C+G` (nvidia-smi pmon)
    pub kind: String,
    /// SM (shader) and memory-bus utilisation over pmon's last interval (0..1)
    pub sm: Option<f64>,
    pub mem_util: Option<f64>,
    /// encoder / decoder utilisation (0..1)
    pub enc: Option<f64>,
    pub dec: Option<f64>,
    /// average SM utilisation over every pmon sample of it this stream saw (0..1)
    pub sm_avg: Option<f64>,
    /// the process's CPU now and over its lifetime (fractions of one core),
    /// its age in seconds and its command line — from the process table
    pub cpu: Option<f64>,
    pub avg_cpu: Option<f64>,
    pub age: Option<u64>,
    pub cmd: String,
    /// the folder it runs in (Linux), to tell whose it is
    pub dir: String,
    /// the agent that started it: an agent process among its ancestors
    pub agent: Option<&'static str>,
    /// the session it's from (filled in by the dashboard: `owner`)
    pub owner: String,
}

#[derive(Clone, Debug, Default)]
pub struct Proc {
    pub pid: String,
    pub user: String,
    /// fraction of one core (1.0 = 100%, can exceed 1 for multi-threaded)
    pub cpu: f64,
    pub rss: f64,
    pub name: String,
    /// R running · S sleeping · D disk wait · Z zombie · T stopped
    pub state: String,
    pub threads: u32,
    /// full command line (when known)
    pub cmd: String,
    /// total CPU time used, seconds
    pub time: f64,
    /// parent pid (the process tree), empty when unknown
    pub ppid: String,
    /// seconds since the process started (uptime), when known
    pub age: Option<u64>,
    /// average CPU over its lifetime (time / age), fraction of one core
    pub avg_cpu: Option<f64>,
}

/// One running agent process (see `Stats::agent_list`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentProc {
    /// registry id
    pub agent: String,
    pub pid: String,
    pub user: String,
    /// seconds since it started
    pub age: Option<u64>,
    pub cmd: String,
    /// started by herdr (a pane of its server): herdr lists it itself
    pub in_herdr: bool,
    /// its CPU now (a fraction of one core) and its memory (bytes)
    pub cpu: f64,
    pub rss: f64,
}

/// One network interface.
#[derive(Clone, Debug, Default)]
pub struct Net {
    pub name: String,
    /// bytes/s
    pub rx: f64,
    pub tx: f64,
    /// (rx+tx) history, bytes/s
    pub hist: Vec<f64>,
    /// received / sent histories, bytes/s
    pub rx_hist: Vec<f64>,
    pub tx_hist: Vec<f64>,
}

/// One block device's throughput.
#[derive(Clone, Debug, Default)]
pub struct DiskIo {
    pub name: String,
    /// bytes/s
    pub read: f64,
    pub write: f64,
    /// fraction of time busy (0..1), Linux only
    pub busy: Option<f64>,
    /// (read+write) history, bytes/s
    pub hist: Vec<f64>,
    /// read / write histories, bytes/s
    pub read_hist: Vec<f64>,
    pub write_hist: Vec<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub hostname: String,
    pub kernel: String,
    pub os: String,
    pub cpu_model: String,
    pub ncpu: usize,
    /// CPU packages (2 on a dual-socket server), 0 when unknown
    pub sockets: usize,
    pub cpu: f64,
    pub cores: Vec<f64>,
    pub cpu_hist: Vec<f64>,
    pub mem_total: f64,
    pub mem_used: f64,
    pub swap_total: f64,
    pub swap_used: f64,
    pub mem_hist: Vec<f64>,
    pub load: [f64; 3],
    pub runnable: String,
    pub uptime: u64,
    pub net_rx: f64,
    pub net_tx: f64,
    pub net_hist: Vec<f64>,
    pub gpus: Vec<Gpu>,
    pub gpu_procs: Vec<GpuProc>,
    pub top: Vec<Proc>,
    pub disks: Vec<(String, f64, f64)>,
    pub claude_procs: usize,
    pub claude_rss: f64,
    pub convos_today: Option<usize>,
    pub samples: u64,
    pub updated: Option<Instant>,
    pub err: Option<String>,
    pub has_gpu_tool: bool,

    // ---- detail for the Monitor view (all optional: whatever the machine offers)
    /// `linux` | `macos` | other `uname -s`
    pub platform: String,
    /// CPU time split of the last sample (fractions of total)
    pub cpu_user: f64,
    pub cpu_system: f64,
    pub cpu_iowait: f64,
    pub cpu_steal: f64,
    /// 1-minute load history
    pub load_hist: Vec<f64>,
    /// package / Tctl temperature (°C)
    pub cpu_temp: Option<f64>,
    /// every temperature sensor worth showing: (label, °C)
    pub temps: Vec<(String, f64)>,
    /// memory breakdown, bytes (Linux: Buffers, Cached+SReclaimable, Shmem, MemAvailable)
    pub mem_buffers: f64,
    pub mem_cached: f64,
    pub mem_shared: f64,
    pub mem_available: f64,
    pub swap_hist: Vec<f64>,
    pub procs_total: usize,
    pub procs_running: usize,
    pub threads_total: usize,
    pub users_logged: Option<usize>,
    /// per interface (loopback / veth / docker bridges left out)
    pub nets: Vec<Net>,
    /// per block device (loop / ram / partitions left out)
    pub disk_io: Vec<DiskIo>,
    /// the process table: top ~60 by CPU, then memory
    pub procs: Vec<Proc>,
    /// why there are no GPU stats (`no nvidia-smi or rocm-smi`, a driver error, …)
    pub gpu_note: Option<String>,
    /// running agent processes by agent id (registry `agents::ALL` executables):
    /// (id, count, total RSS bytes), busiest first. A process whose parent is the
    /// same agent (a node wrapper's native child) adds its RSS but isn't counted.
    pub agent_procs: Vec<(String, usize, f64)>,
    /// each running agent (its helpers left out): what it is, its process, and
    /// whether herdr started it (one of its panes) or it runs outside herdr
    pub agent_list: Vec<AgentProc>,
    /// session logs touched in the last 24 h, per agent whose logs live in a
    /// well-known place (`claude`: ~/.claude/projects, `codex`: ~/.codex/sessions);
    /// only agents whose log folder exists. `convos_today` is the claude count.
    pub sessions_today: Vec<(String, usize)>,

    // ---- histories and extras for the Monitor's charts
    /// seconds between samples, as measured (0 until two have arrived)
    pub period: f64,
    /// CPU time split histories (fractions of total, like `cpu_user`)
    pub cpu_user_hist: Vec<f64>,
    pub cpu_system_hist: Vec<f64>,
    pub cpu_iowait_hist: Vec<f64>,
    /// `cpu_temp` history (°C)
    pub cpu_temp_hist: Vec<f64>,
    /// buffers + page cache history (fraction of total memory)
    pub mem_cache_hist: Vec<f64>,
    /// all interfaces received / sent, bytes/s
    pub net_rx_hist: Vec<f64>,
    pub net_tx_hist: Vec<f64>,
    /// CPU clock (Linux /proc/cpuinfo): the average and the fastest core
    /// now, and the rated maximum, MHz
    pub cpu_mhz: Option<f64>,
    pub cpu_mhz_peak: Option<f64>,
    pub cpu_mhz_max: Option<f64>,
}

impl Stats {
    pub fn mem_frac(&self) -> f64 {
        if self.mem_total > 0.0 {
            self.mem_used / self.mem_total
        } else {
            0.0
        }
    }
    pub fn gpu_util(&self) -> Option<f64> {
        (!self.gpus.is_empty())
            .then(|| self.gpus.iter().map(|g| g.util).sum::<f64>() / self.gpus.len() as f64)
    }
    pub fn live(&self) -> bool {
        self.updated
            .is_some_and(|t| t.elapsed() < Duration::from_secs(15))
    }
}

// ------------------------------------------------------------------ parsing

/// `gpu_note` when a machine simply has no GPU tool: not a failure.
pub const NO_GPU_TOOL: &str = "no nvidia-smi or rocm-smi on this machine";

/// A process as one sample reports it (`Q` on Linux, `T` on macOS).
struct RawProc {
    pid: String,
    ppid: String,
    /// CPU seconds used so far
    secs: Option<f64>,
    rss: f64,
    name: String,
    state: String,
    threads: u32,
    user: String,
    /// ps's own (decaying) %cpu, as a fraction of one core — used until there's a delta
    pcpu: Option<f64>,
    /// seconds since it started
    age: Option<u64>,
}

/// One `nvidia-smi pmon` row.
#[derive(Clone, Debug, Default)]
struct PmonRow {
    gpu: String,
    pid: String,
    kind: String,
    sm: Option<f64>,
    mem: Option<f64>,
    enc: Option<f64>,
    dec: Option<f64>,
    /// frame-buffer MiB
    fb: Option<f64>,
    name: String,
}

/// Process facts other parts of a sample borrow (GPU processes).
#[derive(Clone, Debug, Default)]
struct PInfo {
    cpu: f64,
    avg_cpu: Option<f64>,
    age: Option<u64>,
    user: String,
    cmd: String,
    name: String,
    ppid: String,
}

/// One `Y` line: a hwmon sensor or a thermal zone.
#[derive(Clone, Debug)]
struct RawTemp {
    dir: String,
    chip: String,
    sensor: String,
    label: String,
    c: f64,
}

/// Parser state carried between samples (counter deltas).
#[derive(Default)]
struct Acc {
    cpu_prev: HashMap<String, Vec<u64>>,
    /// pid → CPU seconds at the last sample
    proc_prev: HashMap<String, f64>,
    proc_t: Option<Instant>,
    if_prev: HashMap<String, (u64, u64)>,
    if_t: Option<Instant>,
    disk_prev: HashMap<String, (u64, u64, Option<u64>)>,
    disk_t: Option<Instant>,
    /// pid → (user, command line), from the last `W` batch
    users: HashMap<String, (String, String)>,
    /// pid → threads, from the last `J` batch (macOS)
    threads: HashMap<String, u32>,
    clk_tck: f64,
    page: f64,
    // ---- the sample being assembled
    cores: Vec<f64>,
    saw_stat: bool,
    mac_cpu: Option<(f64, f64, f64)>,
    mem: HashMap<String, f64>,
    nets: Vec<(String, u64, u64)>,
    bdev: Vec<(String, u64, u64, Option<u64>)>,
    temps: Option<Vec<RawTemp>>,
    gpus: Vec<Gpu>,
    rocm: Vec<String>,
    gpu_seen: bool,
    gpu_note: Option<String>,
    gpu_procs: Option<Vec<GpuProc>>,
    procs: Vec<RawProc>,
    users_new: Option<HashMap<String, (String, String)>>,
    threads_new: Option<HashMap<String, u32>>,
    disks: Option<Vec<(String, f64, f64)>>,
    sessions: Option<Vec<(String, usize)>>,
    /// `nvidia-smi pmon` header (column names) and rows of this sample
    pmon_cols: Vec<String>,
    pmon: Vec<PmonRow>,
    pmon_done: bool,
    /// pid → (SM sum, samples): each GPU process's average
    sm_acc: HashMap<String, (f64, u32)>,
    /// pid → facts, from the last process table
    pinfo: HashMap<String, PInfo>,
    /// when the last sample ended
    last_e: Option<Instant>,
    /// this sample's `F` line: average, fastest, rated max (MHz)
    freq: Option<(f64, f64, Option<f64>)>,
}

fn push(h: &mut Vec<f64>, v: f64) {
    h.push(v);
    if h.len() > HIST {
        h.remove(0);
    }
}

fn num(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

fn int(s: &str) -> Option<u64> {
    s.trim().parse::<u64>().ok()
}

/// A tool's "no value": empty, `N/A`, `[N/A]`, `[Not Supported]`, …
fn known(s: &str) -> Option<&str> {
    let s = s.trim();
    (!s.is_empty() && !s.starts_with('[') && !s.eq_ignore_ascii_case("n/a")).then_some(s)
}

/// The first number in a string: `(1700Mhz)` → 1700, `45.0` → 45.
fn first_num(s: &str) -> Option<f64> {
    let s = known(s)?;
    let start = s.find(|c: char| c.is_ascii_digit())?;
    let end = s[start..]
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map_or(s.len(), |e| start + e);
    num(&s[start..end])
}

fn by_num(a: f64, b: f64) -> std::cmp::Ordering {
    b.partial_cmp(&a).unwrap_or(std::cmp::Ordering::Equal)
}

fn base(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// `ps` cputime: `MMM:SS.cc` (macOS), `[DD-]HH:MM:SS` (procps).
fn cputime(s: &str) -> Option<f64> {
    let (days, rest) = match s.trim().split_once('-') {
        Some((d, r)) => (num(d)?, r),
        None => (0.0, s.trim()),
    };
    let mut t = 0.0;
    for p in rest.split(':') {
        t = t * 60.0 + num(p)?;
    }
    Some(days * 86400.0 + t)
}

/// `linux` / `macos` / … from `uname -sr`.
fn platform_of(kernel: &str) -> String {
    match kernel.split_whitespace().next().unwrap_or("") {
        "Linux" => "linux".into(),
        "Darwin" => "macos".into(),
        k => k.to_lowercase(),
    }
}

/// nvidia `clocks_event_reasons.active` / `clocks_throttle_reasons.active` bitmask, in words.
fn throttle_words(mask: &str) -> Vec<String> {
    let Some(h) = known(mask) else {
        return vec![];
    };
    let h = h.trim_start_matches("0x").trim_start_matches("0X");
    let Ok(m) = u64::from_str_radix(h, 16) else {
        return vec![];
    };
    const BITS: &[(u64, &str)] = &[
        (0x2, "app clocks"),
        (0x4, "power cap"),
        (0x8, "HW slowdown"),
        (0x10, "sync boost"),
        (0x20, "thermal (SW)"),
        (0x40, "thermal (HW)"),
        (0x80, "power brake"),
        (0x100, "display clock"),
    ];
    // 0x1 is "GPU idle": not a throttle
    BITS.iter()
        .filter(|(b, _)| m & b != 0)
        .map(|(_, w)| w.to_string())
        .collect()
}

/// One nvidia-smi CSV row (basic 10 fields, or the extended 22).
fn parse_nvidia(csv: &str) -> Option<Gpu> {
    let f: Vec<&str> = csv.split(',').map(str::trim).collect();
    if f.len() < 10 || known(f[0]).is_none() {
        return None;
    }
    let n = |i: usize| f.get(i).and_then(|x| known(x)).and_then(num);
    let s = |i: usize| f.get(i).and_then(|x| known(x)).unwrap_or("").to_string();
    let pcie = match (s(15), s(16)) {
        (g, w) if !g.is_empty() && !w.is_empty() => format!("gen{g} x{w}"),
        (g, _) if !g.is_empty() => format!("gen{g}"),
        (_, w) if !w.is_empty() => format!("x{w}"),
        _ => String::new(),
    };
    Some(Gpu {
        index: f[0].into(),
        name: f[1].replace("NVIDIA ", "").replace("GeForce ", ""),
        util: n(2).unwrap_or(0.0) / 100.0,
        mem_used: n(3).unwrap_or(0.0) * 1048576.0,
        mem_total: n(4).unwrap_or(0.0) * 1048576.0,
        temp: n(5),
        power: n(6),
        power_limit: n(7),
        fan: n(8),
        bus: s(9),
        vendor: "nvidia".into(),
        mem_util: n(10).map(|v| v / 100.0),
        clock_sm: n(11),
        clock_mem: n(12),
        clock_sm_max: n(13),
        pstate: s(14),
        pcie,
        enc: n(17).map(|v| v / 100.0),
        dec: n(18).map(|v| v / 100.0),
        temp_mem: n(19),
        throttle: f.get(20).map(|m| throttle_words(m)).unwrap_or_default(),
        ecc: f.get(21).and_then(|x| known(x)).and_then(int),
        ..Default::default()
    })
}

/// An Apple GPU's `Z` record (model, cores, device %, renderer %, tiler %,
/// in use, allocated, the Mac's memory) as the `n`th GPU.
fn parse_apple(f: &[&str], n: usize) -> Gpu {
    let num = |i: usize| f.get(i).and_then(|x| x.trim().parse::<f64>().ok());
    let pct = |i: usize| num(i).map(|v| (v / 100.0).clamp(0.0, 1.0));
    let model = f
        .first()
        .map(|m| m.trim())
        .filter(|m| !m.is_empty())
        .unwrap_or("Apple GPU");
    let cores = num(1).map(|c| c as u32).filter(|c| *c > 0);
    // `M4 · 10 cores` (its badge says Apple already)
    let short = model.strip_prefix("Apple ").unwrap_or(model);
    Gpu {
        index: n.to_string(),
        name: match cores {
            Some(c) => format!("{short} · {c} cores"),
            None => short.to_string(),
        },
        util: pct(2).unwrap_or(0.0),
        mem_used: num(5).unwrap_or(0.0),
        mem_total: num(7).unwrap_or(0.0),
        vendor: crate::logo::gpu_vendor(model)
            .unwrap_or("apple")
            .to_string(),
        renderer: pct(3),
        tiler: pct(4),
        cores,
        mem_alloc: num(6),
        ..Default::default()
    }
}

/// A CSV row, honouring double quotes.
fn csv_cells(l: &str) -> Vec<String> {
    let (mut out, mut cur, mut q) = (Vec::new(), String::new(), false);
    for ch in l.chars() {
        match ch {
            '"' => q = !q,
            ',' if !q => out.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(ch),
        }
    }
    out.push(cur.trim().to_string());
    out
}

/// rocm-smi `--csv` output: one or more `device,…` header rows, each followed by
/// `cardN,…` rows. Column names differ between ROCm versions, so look them up
/// by what they contain.
fn parse_rocm(lines: &[String]) -> Vec<Gpu> {
    let mut hdr: Vec<String> = Vec::new();
    let mut cards: Vec<(String, HashMap<String, String>)> = Vec::new();
    for l in lines {
        let cells = csv_cells(l);
        if cells[0].eq_ignore_ascii_case("device") {
            hdr = cells.iter().map(|c| c.to_lowercase()).collect();
            continue;
        }
        if hdr.is_empty() || !cells[0].starts_with("card") {
            continue;
        }
        let i = match cards.iter().position(|c| c.0 == cells[0]) {
            Some(i) => i,
            None => {
                cards.push((cells[0].clone(), HashMap::new()));
                cards.len() - 1
            }
        };
        for (h, v) in hdr.iter().zip(cells.iter()).skip(1) {
            cards[i].1.insert(h.clone(), v.clone());
        }
    }
    cards
        .into_iter()
        .map(|(card, m)| {
            // the first column whose name contains every word of a pattern (and none of `not`)
            let get = |pats: &[&str], not: &str| -> Option<&String> {
                pats.iter().find_map(|p| {
                    m.iter()
                        .filter(|(k, v)| {
                            k.contains(p)
                                && (not.is_empty() || !k.contains(not))
                                && known(v).is_some()
                        })
                        .min_by_key(|(k, _)| k.len())
                        .map(|(_, v)| v)
                })
            };
            let n = |pats: &[&str], not: &str| get(pats, not).and_then(|v| first_num(v));
            let power = n(&["graphics package power"], "max");
            Gpu {
                index: card.trim_start_matches("card").to_string(),
                name: get(&["card series", "card model", "device name"], "")
                    .cloned()
                    .unwrap_or_else(|| "AMD GPU".into()),
                util: n(&["gpu use (%)"], "").unwrap_or(0.0) / 100.0,
                mem_used: n(&["vram total used memory"], "").unwrap_or(0.0),
                mem_total: n(&["vram total memory"], "").unwrap_or(0.0),
                temp: n(
                    &["(sensor edge)", "(sensor junction)", "temperature"],
                    "memory",
                ),
                temp_mem: n(&["(sensor memory)"], ""),
                power,
                power_limit: n(&["max graphics package power"], ""),
                fan: n(&["fan speed (%)"], ""),
                bus: get(&["pci bus"], "").cloned().unwrap_or_default(),
                vendor: "amd".into(),
                mem_util: n(
                    &[
                        "memory read/write activity",
                        "gpu memory use (%)",
                        "memory activity",
                    ],
                    "",
                )
                .map(|v| v / 100.0),
                clock_sm: n(&["sclk clock speed"], ""),
                clock_mem: n(&["mclk clock speed"], ""),
                ..Default::default()
            }
        })
        .collect()
}

/// CPU chips' packages, cores, and every other sensor worth a line, from the
/// hwmon readings; thermal zones only when hwmon has nothing.
fn temps_from(raw: &[RawTemp]) -> (Option<f64>, Vec<(String, f64)>) {
    // sane readings only: unconnected inputs report 0, -273, 127, 255…
    let ok = |c: f64| c > 0.5 && c < 127.0;
    let idx = |s: &str| {
        s.trim_start_matches(|c: char| !c.is_ascii_digit())
            .parse::<u32>()
            .unwrap_or(0)
    };
    let mut hw: Vec<&RawTemp> = raw.iter().filter(|t| t.chip != "zone" && ok(t.c)).collect();
    hw.sort_by_key(|t| (idx(&t.dir), idx(&t.sensor)));
    let mut zones: Vec<&RawTemp> = raw.iter().filter(|t| t.chip == "zone" && ok(t.c)).collect();
    zones.sort_by_key(|t| idx(&t.dir));

    let (mut pkgs, mut cores, mut cpu_rows, mut rest) = (vec![], vec![], vec![], vec![]);
    let (mut nvme, mut drive) = (0, 0);
    let mut dirs: Vec<&str> = hw.iter().map(|t| t.dir.as_str()).collect();
    dirs.dedup();
    for d in dirs {
        let s: Vec<&&RawTemp> = hw.iter().filter(|t| t.dir == d).collect();
        let chip = s[0].chip.as_str();
        let lab = |l: &str| s.iter().find(|t| t.label == l).map(|t| t.c);
        let max = s.iter().map(|t| t.c).fold(f64::MIN, f64::max);
        match chip {
            "coretemp" => {
                for t in &s {
                    if t.label.starts_with("Package") {
                        pkgs.push(t.c);
                    } else if t.label.starts_with("Core") {
                        cores.push(t.c);
                    }
                }
            }
            "k10temp" | "zenpower" => {
                if let Some(c) = lab("Tdie").or_else(|| lab("Tctl")) {
                    pkgs.push(c);
                }
                for t in s.iter().filter(|t| t.label.starts_with("Tccd")) {
                    cpu_rows.push((t.label.replace("Tccd", "ccd"), t.c));
                }
            }
            "k8temp" | "cpu_thermal" | "cpu-thermal" | "soc_thermal" => pkgs.push(max),
            "nvme" => {
                let c = lab("Composite").unwrap_or(s[0].c);
                rest.push((format!("nvme{nvme}"), c));
                nvme += 1;
            }
            "drivetemp" => {
                rest.push((format!("disk{drive}"), max));
                drive += 1;
            }
            "amdgpu" | "nouveau" | "radeon" => {
                for t in &s {
                    let l = if t.label.is_empty() { "temp" } else { &t.label };
                    rest.push((format!("gpu {l}"), t.c));
                }
            }
            "acpitz" => rest.push(("acpi".into(), max)),
            c if c.starts_with("iwlwifi") || c.starts_with("ath") || c.starts_with("mt7") => {
                rest.push(("wifi".into(), max))
            }
            // network PHYs report as their interface (`eno1`): one line
            c if s
                .iter()
                .any(|t| t.label.contains("PHY") || t.label.contains("MAC")) =>
            {
                rest.push((c.to_string(), max))
            }
            c => {
                for t in &s {
                    let name = if c.is_empty() { "sensor" } else { c };
                    if t.label.is_empty() {
                        rest.push((name.to_string(), t.c));
                    } else {
                        rest.push((format!("{name} {}", t.label), t.c));
                    }
                }
            }
        }
    }
    let zone_cpu = || {
        zones
            .iter()
            .filter(|z| {
                let l = z.label.to_lowercase();
                l.contains("pkg") || l.contains("cpu") || l.contains("soc")
            })
            .map(|z| z.c)
            .reduce(f64::max)
    };
    let cpu_temp = pkgs
        .iter()
        .copied()
        .reduce(f64::max)
        .or_else(|| cores.iter().copied().reduce(f64::max))
        .or_else(zone_cpu);
    let mut out: Vec<(String, f64)> = Vec::new();
    match pkgs.len() {
        0 => {}
        1 => out.push(("cpu".into(), pkgs[0])),
        _ => out.extend(
            pkgs.iter()
                .enumerate()
                .map(|(i, c)| (format!("cpu{i}"), *c)),
        ),
    }
    if let Some(c) = cores.iter().copied().reduce(f64::max) {
        out.push(("cores max".into(), c));
    }
    out.extend(cpu_rows);
    out.extend(rest);
    if hw.is_empty() {
        out.extend(zones.iter().map(|z| {
            let l = match z.label.as_str() {
                "x86_pkg_temp" => "cpu".to_string(),
                "" => z.dir.clone(),
                l => l.to_string(),
            };
            (l, z.c)
        }));
    }
    // one line per label
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (l, _) in out.iter_mut() {
        let k = seen.entry(l.clone()).or_insert(0);
        *k += 1;
        if *k > 1 {
            *l = format!("{l} {k}");
        }
    }
    out.truncate(16);
    (cpu_temp, out)
}

/// Interfaces not worth a line: loopback, container/VM plumbing, Apple
/// peer-to-peer links, and anything that never both sent and received (down
/// NICs, idle tunnels).
fn skip_iface(name: &str, rx: u64, tx: u64) -> bool {
    const VIRT: &[&str] = &[
        "lo", "veth", "docker", "br-", "virbr", "vnet", "cali", "flannel", "cni", "tunl", "weave",
        "kube-", "vxlan", "awdl", "llw", "anpi", "gif", "stf",
    ];
    VIRT.iter().any(|p| name.starts_with(p)) || rx == 0 || tx == 0
}

/// Interpreters an agent may run under: `node /usr/local/bin/gemini`.
fn interpreter(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    matches!(
        n.as_str(),
        "node" | "nodejs" | "bun" | "deno" | "mainthread"
    ) || n.starts_with("python")
}

/// Exact, case-sensitive: CLI executables are lowercase, while app helpers
/// (`…/MacOS/Codex (Service)`, whose argv[0] splits at the space) aren't agents.
fn by_exe(s: &str) -> Option<&'static str> {
    agents::ALL
        .iter()
        .find(|a| a.exes.contains(&s))
        .map(|a| a.id)
}

/// An npm package (`@google/gemini-cli`, `opencode-ai`, `@kilocode/cli`) → agent.
fn by_package(pkg: &str) -> Option<&'static str> {
    let (scope, name) = match pkg.strip_prefix('@').and_then(|p| p.split_once('/')) {
        Some((s, n)) => (Some(s), n),
        None => (None, pkg),
    };
    let mut cands = vec![name];
    if let Some(s) = scope.filter(|_| matches!(name, "cli" | "code" | "agent")) {
        cands.push(s);
    }
    let mut all: Vec<String> = Vec::new();
    for c in cands {
        let c = c.to_ascii_lowercase();
        all.push(c.clone());
        for suf in ["-coding-agent", "-agent", "-cli", "-code", "-ai"] {
            if let Some(x) = c.strip_suffix(suf) {
                all.push(x.to_string());
            }
        }
    }
    all.iter().find_map(|c| {
        agents::ALL
            .iter()
            .find(|a| a.exes.contains(&c.as_str()) || a.aliases.contains(&c.as_str()))
            .map(|a| a.id)
    })
}

/// Which agent (registry id) a process is: its name, else argv[0], else — for
/// `node`/`bun`/`python`… — the script it runs (`…/bin/gemini`,
/// `…/node_modules/@anthropic-ai/claude-code/cli.js`).
/// The agent among a process's ancestors (its parent, theirs, …), if any.
fn agent_above(pinfo: &HashMap<String, PInfo>, pid: &str) -> Option<&'static str> {
    let mut cur = pinfo.get(pid)?.ppid.clone();
    for _ in 0..32 {
        let p = pinfo.get(&cur)?;
        if let Some(a) = agent_of(&p.name, &p.cmd) {
            return Some(a);
        }
        if p.ppid.is_empty() || p.ppid == cur || p.ppid == "0" || p.ppid == "1" {
            return None;
        }
        cur = p.ppid.clone();
    }
    None
}

/// Whose GPU process it is: the session on its machine working in the
/// deepest folder that holds the process's own (one of the agent that started
/// it first); None for a process no session's folder holds.
pub fn owner<'a>(
    p: &GpuProc,
    sessions: &'a [crate::herdr::Session],
) -> Option<&'a crate::herdr::Session> {
    if p.dir.is_empty() {
        return None;
    }
    let under = |cwd: &str| {
        let c = cwd.trim_end_matches('/');
        !c.is_empty() && (p.dir == c || p.dir.starts_with(&format!("{c}/")))
    };
    sessions
        .iter()
        .filter(|s| s.agent != crate::agents::SHELL && under(&s.cwd))
        .max_by_key(|s| {
            (
                Some(s.agent.as_str()) == p.agent,
                s.cwd.trim_end_matches('/').len(),
            )
        })
}

pub fn agent_of(name: &str, cmd: &str) -> Option<&'static str> {
    if let Some(id) = by_exe(base(name)) {
        return Some(id);
    }
    let mut words = cmd.split_whitespace();
    let argv0 = base(words.next().unwrap_or(""));
    if let Some(id) = by_exe(argv0) {
        return Some(id);
    }
    if !(interpreter(base(name)) || interpreter(argv0)) {
        return None;
    }
    let script = words.find(|w| !w.starts_with('-'))?;
    let stem = base(script);
    let stem = [".js", ".mjs", ".cjs", ".ts", ".py"]
        .iter()
        .find_map(|e| stem.strip_suffix(e))
        .unwrap_or(stem);
    if let Some(id) = by_exe(stem) {
        return Some(id);
    }
    let (_, rest) = script.split_once("node_modules/")?;
    let mut parts = rest.split('/');
    let first = parts.next()?;
    let pkg = if first.starts_with('@') {
        format!("{first}/{}", parts.next().unwrap_or(""))
    } else {
        first.to_string()
    };
    by_package(&pkg)
}

/// Is `pid` (a descendant of) a herdr process? Walks its parents.
fn under_herdr(pid: &str, parents: &HashMap<String, (String, String)>) -> bool {
    let mut cur = pid.to_string();
    for _ in 0..40 {
        let Some((pp, _)) = parents.get(&cur) else {
            return false;
        };
        if pp.is_empty() || pp == "0" || pp == "1" || *pp == cur {
            return false;
        }
        match parents.get(pp) {
            Some((_, name)) if base(name).contains("herdr") => return true,
            Some(_) => cur = pp.clone(),
            None => return false,
        }
    }
    false
}

/// Command lines of an agent's helper modes, not sessions: a browser's native
/// host, an MCP or app server, Claude Code's background daemon and the
/// terminals it hosts for background sessions.
const HELPER_ARGS: &[&str] = &[
    "--chrome-native-host",
    "--native-host",
    " mcp serve",
    " mcp-server",
    " app-server",
    " daemon run",
];

fn helper(cmd: &str) -> bool {
    let c = format!(" {cmd}");
    HELPER_ARGS.iter().any(|h| c.contains(h))
        // Claude Code's background modes: --bg-pty-host, --bg-spare, …
        || cmd.split_whitespace().any(|a| a.starts_with("--bg-"))
        // Codex running a command in its sandbox (the ChatGPT app's tools do)
        || matches!(cmd.split_whitespace().nth(1), Some("sandbox" | "debug"))
}

/// Each agent, its helpers (children that are the same agent, or a helper
/// mode) left out (pure — tested). A session a helper started (a background
/// session under Claude Code's terminal host) is a session of its own.
fn agent_list(
    procs: &[(String, String, f64, &'static str)],
    parents: &HashMap<String, (String, String)>,
    info: &HashMap<String, PInfo>,
) -> Vec<AgentProc> {
    let of: HashMap<&str, &'static str> = procs
        .iter()
        .map(|(pid, _, _, a)| (pid.as_str(), *a))
        .collect();
    let is_helper = |pid: &str| info.get(pid).is_some_and(|i| helper(&i.cmd));
    let mut v: Vec<AgentProc> = procs
        .iter()
        .filter(|(_, ppid, _, a)| of.get(ppid.as_str()) != Some(a) || is_helper(ppid))
        .filter(|(pid, _, _, _)| !is_helper(pid))
        .map(|(pid, _, rss, a)| {
            let i = info.get(pid);
            AgentProc {
                agent: a.to_string(),
                pid: pid.clone(),
                user: i.map(|i| i.user.clone()).unwrap_or_default(),
                age: i.and_then(|i| i.age),
                cmd: i.map(|i| i.cmd.clone()).unwrap_or_default(),
                in_herdr: under_herdr(pid, parents),
                cpu: i.map_or(0.0, |i| i.cpu),
                rss: *rss,
            }
        })
        .collect();
    // the longest-running first (a stable order)
    v.sort_by(|a, b| b.age.cmp(&a.age).then(a.pid.cmp(&b.pid)));
    v
}

/// Agent processes: (id, count, rss), busiest first. A process whose parent is
/// the same agent (wrapper → native binary, a helper) adds RSS but no count.
fn agent_table(procs: &[(String, String, f64, &'static str)]) -> Vec<(String, usize, f64)> {
    let of: HashMap<&str, &'static str> = procs
        .iter()
        .map(|(pid, _, _, a)| (pid.as_str(), *a))
        .collect();
    let mut t: Vec<(String, usize, f64)> = Vec::new();
    for (_, ppid, rss, a) in procs {
        let child = of.get(ppid.as_str()) == Some(a);
        match t.iter_mut().find(|x| x.0 == *a) {
            Some(x) => {
                x.1 += usize::from(!child);
                x.2 += rss;
            }
            None => t.push((a.to_string(), usize::from(!child), *rss)),
        }
    }
    t.retain(|x| x.1 > 0);
    t.sort_by(|a, b| b.1.cmp(&a.1).then(by_num(a.2, b.2)));
    t
}

/// The CPU's model as people write it: no (R)/(TM)/"CPU"/"@ 2.10GHz", and
/// not shouted (`INTEL(R) XEON(R) GOLD 6544Y` → `Intel Xeon Gold 6544Y`).
fn cpu_name(raw: &str) -> String {
    let n = raw
        .replace("(R)", "")
        .replace("(TM)", "")
        .replace(" CPU", "")
        .split(" @ ")
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if n.chars().any(|c| c.is_lowercase()) {
        return n;
    }
    n.split(' ')
        .map(|w| {
            // model numbers and acronyms stay as they are (6544Y, AMD, EPYC)
            const KEEP: [&str; 8] = ["AMD", "EPYC", "ARM", "IBM", "APU", "SOC", "AWS", "NVIDIA"];
            if w.chars().any(|c| c.is_ascii_digit()) || KEEP.contains(&w) {
                return w.to_string();
            }
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_string() + &c.as_str().to_lowercase())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl Acc {
    fn line(&mut self, st: &mut Stats, l: &str) {
        self.line_at(st, l, Instant::now())
    }

    fn line_at(&mut self, st: &mut Stats, l: &str, now: Instant) {
        let f: Vec<&str> = l.split('\t').collect();
        let g = |i: usize| f.get(i).copied().unwrap_or("");
        match f[0] {
            "H" if f.len() >= 4 => {
                st.hostname = f[1].into();
                st.kernel = f[2].into();
                st.os = f[3].into();
                st.platform = platform_of(f[2]);
            }
            "C" if f.len() >= 3 => {
                st.ncpu = f[1].trim().parse().unwrap_or(0);
                st.cpu_model = cpu_name(f[2]);
                self.clk_tck = num(g(3)).filter(|v| *v > 0.0).unwrap_or(0.0);
                self.page = num(g(4)).filter(|v| *v > 0.0).unwrap_or(0.0);
                st.sockets = g(5).trim().parse().unwrap_or(0);
            }
            "S" if f.len() >= 2 => {
                let mut it = f[1].split_whitespace();
                let name = it.next().unwrap_or("").to_string();
                let v: Vec<u64> = it.filter_map(|x| x.parse().ok()).collect();
                if v.len() < 4 {
                    return;
                }
                let at = |v: &[u64], i: usize| v.get(i).copied().unwrap_or(0);
                let idle = |v: &[u64]| at(v, 3) + at(v, 4);
                let total = |v: &[u64]| v.iter().take(8).sum::<u64>();
                let prev = self.cpu_prev.insert(name.clone(), v.clone());
                let (frac, dt) = match &prev {
                    Some(p) if total(&v) > total(p) => {
                        let dt = (total(&v) - total(p)) as f64;
                        (1.0 - idle(&v).saturating_sub(idle(p)) as f64 / dt, dt)
                    }
                    _ => (0.0, 0.0),
                };
                if name == "cpu" {
                    self.saw_stat = true;
                    st.cpu = frac.clamp(0.0, 1.0);
                    if let Some(p) = prev.filter(|_| dt > 0.0) {
                        // user+nice, system+irq+softirq, iowait, steal
                        let d = |is: &[usize]| {
                            is.iter()
                                .map(|&i| at(&v, i).saturating_sub(at(&p, i)))
                                .sum::<u64>() as f64
                                / dt
                        };
                        st.cpu_user = d(&[0, 1]);
                        st.cpu_system = d(&[2, 5, 6]);
                        st.cpu_iowait = d(&[4]);
                        st.cpu_steal = d(&[7]);
                    }
                } else {
                    self.cores.push(frac.clamp(0.0, 1.0));
                }
            }
            "c" if f.len() >= 4 => {
                if let (Some(u), Some(s), Some(i)) = (num(f[1]), num(f[2]), num(f[3])) {
                    self.mac_cpu = Some((u, s, i));
                }
            }
            "M" if f.len() >= 3 => {
                if let Some(v) = num(f[2]) {
                    self.mem
                        .insert(f[1].trim().trim_end_matches(':').to_string(), v * 1024.0);
                }
            }
            "L" if f.len() >= 2 => {
                let v: Vec<f64> = f[1].split_whitespace().filter_map(num).collect();
                if v.len() >= 3 {
                    st.load = [v[0], v[1], v[2]];
                }
                st.runnable = g(2).trim().to_string();
            }
            "U" if f.len() >= 2 => st.uptime = f[1].trim().parse().unwrap_or(0),
            "N" if f.len() >= 4 => {
                if let (Some(r), Some(t)) = (int(f[2]), int(f[3])) {
                    self.nets.push((f[1].trim().to_string(), r, t));
                }
            }
            "B" if f.len() >= 4 => {
                if let (Some(r), Some(w)) = (int(f[2]), int(f[3])) {
                    self.bdev.push((f[1].to_string(), r, w, int(g(4))));
                }
            }
            "Y" if f.len() >= 6 => {
                if let Some(m) = num(f[5]) {
                    self.temps.get_or_insert_with(Vec::new).push(RawTemp {
                        dir: f[1].into(),
                        chip: f[2].trim().into(),
                        sensor: f[3].into(),
                        label: f[4].trim().into(),
                        c: m / 1000.0,
                    });
                }
            }
            "Q" if f.len() >= 5 => {
                // ticks: `utime+stime` (or one number)
                let ticks: f64 = f[2].split('+').filter_map(num).sum();
                let clk = if self.clk_tck > 0.0 {
                    self.clk_tck
                } else {
                    100.0
                };
                let page = if self.page > 0.0 { self.page } else { 4096.0 };
                // start time: clock ticks after boot (uptime came earlier in the sample)
                let age = num(g(8))
                    .filter(|_| st.uptime > 0)
                    .map(|t| (st.uptime as f64 - t / clk).max(0.0) as u64);
                self.procs.push(RawProc {
                    pid: f[1].into(),
                    ppid: g(7).into(),
                    secs: Some(ticks / clk),
                    rss: num(f[3]).unwrap_or(0.0) * page,
                    name: f[4].into(),
                    state: g(5).chars().next().map(String::from).unwrap_or_default(),
                    threads: g(6).trim().parse().unwrap_or(0),
                    user: String::new(),
                    pcpu: None,
                    age,
                });
            }
            "T" if f.len() >= 6 => {
                let comm = f[5];
                let state = match g(6).chars().next() {
                    Some('I') => "S".to_string(), // macOS: idle (sleeping > 20 s)
                    Some('U') => "D".to_string(), // macOS: uninterruptible wait
                    Some(c) => c.to_string(),
                    None => String::new(),
                };
                self.procs.push(RawProc {
                    pid: f[1].into(),
                    ppid: g(8).into(),
                    secs: cputime(g(7)),
                    rss: num(f[4]).unwrap_or(0.0) * 1024.0,
                    name: if comm.starts_with('/') {
                        base(comm)
                    } else {
                        comm
                    }
                    .to_string(),
                    state,
                    threads: 0,
                    user: f[2].into(),
                    pcpu: num(f[3]).map(|v| v / 100.0),
                    age: cputime(g(9)).map(|s| s as u64),
                });
            }
            "J" if f.len() >= 3 => {
                if let Ok(t) = f[2].trim().parse() {
                    self.threads_new
                        .get_or_insert_with(HashMap::new)
                        .insert(f[1].into(), t);
                }
            }
            "W" if f.len() >= 3 => {
                self.users_new.get_or_insert_with(HashMap::new).insert(
                    f[1].trim().to_string(),
                    (f[2].trim().to_string(), g(3).to_string()),
                );
            }
            "G" if f.len() >= 2 => {
                if let Some(x) = parse_nvidia(&l[2..]) {
                    self.gpus.push(x);
                }
            }
            "R" if f.len() >= 2 => self.rocm.push(l[2..].to_string()),
            "Z" if f.len() >= 9 => {
                let i = self.gpus.len();
                self.gpus.push(parse_apple(&f[1..], i));
            }
            "F" if f.len() >= 3 => {
                if let (Some(a), Some(m)) = (num(f[1]), num(f[2])) {
                    let max = num(g(3)).map(|k| k / 1000.0).filter(|m| *m > 0.0);
                    self.freq = Some((a, m, max));
                }
            }
            "P" if f.len() >= 2 => self.pmon_line(&l[2..]),
            "p" => self.pmon_done = true,
            "g" => self.gpu_seen = true,
            "X" if f.len() >= 2 => {
                let msg = g(2).trim();
                self.gpu_note = Some(match f[1] {
                    "none" if st.platform == "macos" => {
                        "this Mac's GPU didn't report its statistics (ioreg)".into()
                    }
                    "none" => NO_GPU_TOOL.into(),
                    "amd" if msg.is_empty() => "rocm-smi failed".into(),
                    _ if msg.is_empty() => "nvidia-smi failed".into(),
                    _ => msg.to_string(),
                });
            }
            "A" if f.len() >= 6 => {
                self.gpu_procs.get_or_insert_with(Vec::new).push(GpuProc {
                    gpu: f[1].trim().to_string(),
                    pid: f[2].trim().into(),
                    mem: num(f[3]).unwrap_or(0.0) * 1048576.0,
                    user: f[4].trim().into(),
                    name: base(f[5]).to_string(),
                    kind: "C".into(),
                    ..Default::default()
                });
            }
            "V" if f.len() >= 3 => {
                if let Some(p) = self
                    .gpu_procs
                    .as_mut()
                    .and_then(|v| v.iter_mut().find(|p| p.pid == f[1].trim()))
                {
                    p.dir = f[2].trim_end().to_string();
                }
            }
            "a" => {
                self.gpu_procs.get_or_insert_with(Vec::new);
            }
            "D" if f.len() >= 4 => {
                let d = self.disks.get_or_insert_with(Vec::new);
                if !d.iter().any(|x| x.0 == f[1]) {
                    d.push((
                        f[1].into(),
                        num(f[2]).unwrap_or(0.0) * 1024.0,
                        num(f[3]).unwrap_or(0.0) * 1024.0,
                    ));
                }
            }
            "O" if f.len() >= 2 => st.users_logged = f[1].trim().parse().ok(),
            "K" if f.len() >= 2 => {
                let Ok(n) = f[1].trim().parse::<usize>() else {
                    return;
                };
                let agent = match g(2).trim() {
                    "" => "claude",
                    a => a,
                };
                if agent == "claude" {
                    st.convos_today = Some(n);
                }
                self.sessions
                    .get_or_insert_with(Vec::new)
                    .push((agent.to_string(), n));
            }
            "E" => self.finish(st, now),
            _ => {}
        }
    }

    /// `nvidia-smi pmon` output: `# gpu pid type sm mem … command` names the
    /// columns (they vary between drivers), `-` is no value, and the command
    /// (which may hold spaces) is the rest of the line.
    fn pmon_line(&mut self, l: &str) {
        let t = l.trim();
        if let Some(h) = t.strip_prefix('#') {
            let cols: Vec<String> = h.split_whitespace().map(|x| x.to_lowercase()).collect();
            if cols.iter().any(|c| c == "pid") {
                self.pmon_cols = cols;
            }
            return;
        }
        let cols = &self.pmon_cols;
        if cols.is_empty() {
            return;
        }
        let w: Vec<&str> = t.split_whitespace().collect();
        let ncol = cols.len();
        if w.len() < ncol.saturating_sub(1) {
            return;
        }
        let at = |name: &str| cols.iter().position(|c| c == name).and_then(|i| w.get(i));
        let pct = |name: &str| at(name).and_then(|v| num(v)).map(|v| v / 100.0);
        let Some(pid) = at("pid").filter(|p| p.chars().all(|c| c.is_ascii_digit())) else {
            return;
        };
        let name = if cols.last().is_some_and(|c| c == "command") && w.len() >= ncol {
            w[ncol - 1..].join(" ")
        } else {
            String::new()
        };
        self.pmon.push(PmonRow {
            gpu: at("gpu").map(|s| s.to_string()).unwrap_or_default(),
            pid: pid.to_string(),
            kind: at("type").map(|s| s.to_string()).unwrap_or_default(),
            sm: pct("sm"),
            mem: pct("mem"),
            enc: pct("enc"),
            dec: pct("dec"),
            fb: at("fb").and_then(|v| num(v)),
            name,
        });
    }

    fn finish(&mut self, st: &mut Stats, now: Instant) {
        st.samples += 1;
        st.updated = Some(now);
        st.err = None;
        if let Some(t) = self.last_e.replace(now) {
            let dt = now.duration_since(t).as_secs_f64();
            if dt > 0.0 {
                st.period = if st.period > 0.0 {
                    st.period * 0.8 + dt * 0.2
                } else {
                    dt
                };
            }
        }
        if let Some(u) = self.users_new.take() {
            self.users = u;
        }
        if let Some(t) = self.threads_new.take() {
            self.threads = t;
        }
        self.cpu(st);
        push(&mut st.cpu_hist, st.cpu);
        push(&mut st.cpu_user_hist, st.cpu_user);
        push(&mut st.cpu_system_hist, st.cpu_system);
        push(&mut st.cpu_iowait_hist, st.cpu_iowait);
        if let Some((a, m, max)) = self.freq.take() {
            st.cpu_mhz = Some(a);
            st.cpu_mhz_peak = Some(m);
            st.cpu_mhz_max = max.or(st.cpu_mhz_max);
        }
        st.cores = std::mem::take(&mut self.cores);
        push(&mut st.load_hist, st.load[0]);
        self.memory(st);
        self.network(st, now);
        self.disk_io(st, now);
        if let Some(raw) = self.temps.take() {
            (st.cpu_temp, st.temps) = temps_from(&raw);
        }
        if let Some(t) = st.cpu_temp {
            push(&mut st.cpu_temp_hist, t);
        }
        self.processes(st, now);
        self.gpu(st);
        if let Some(d) = self.disks.take() {
            st.disks = d;
        }
        if let Some(s) = self.sessions.take() {
            st.sessions_today = s;
        }
    }

    /// Linux set it from `S` lines; macOS from `c` (iostat over the pause), or
    /// ps's %cpu for the very first sample.
    fn cpu(&mut self, st: &mut Stats) {
        if std::mem::take(&mut self.saw_stat) {
            return;
        }
        if let Some((u, s, idle)) = self.mac_cpu.take() {
            st.cpu = (1.0 - idle / 100.0).clamp(0.0, 1.0);
            st.cpu_user = (u / 100.0).clamp(0.0, 1.0);
            st.cpu_system = (s / 100.0).clamp(0.0, 1.0);
        } else if st.samples == 1 && st.ncpu > 0 {
            let sum: f64 = self.procs.iter().filter_map(|p| p.pcpu).sum();
            st.cpu = (sum / st.ncpu as f64).clamp(0.0, 1.0);
        }
    }

    fn memory(&mut self, st: &mut Stats) {
        let mem = std::mem::take(&mut self.mem);
        let m = |k: &str| mem.get(k).copied();
        let z = |k: &str| m(k).unwrap_or(0.0);
        let Some(total) = m("MemTotal").filter(|t| *t > 0.0) else {
            return;
        };
        let avail = m("MemAvailable").unwrap_or_else(|| z("MemFree") + z("Buffers") + z("Cached"));
        st.mem_total = total;
        st.mem_available = avail.min(total);
        st.mem_used = (total - avail).max(0.0);
        st.mem_buffers = z("Buffers");
        st.mem_cached = z("Cached") + z("SReclaimable");
        st.mem_shared = z("Shmem");
        st.swap_total = z("SwapTotal");
        st.swap_used = (st.swap_total - m("SwapFree").unwrap_or(st.swap_total)).max(0.0);
        let fr = st.mem_frac();
        push(&mut st.mem_hist, fr);
        push(
            &mut st.mem_cache_hist,
            ((st.mem_buffers + st.mem_cached) / total).clamp(0.0, 1.0),
        );
        let sw = if st.swap_total > 0.0 {
            st.swap_used / st.swap_total
        } else {
            0.0
        };
        push(&mut st.swap_hist, sw);
    }

    fn network(&mut self, st: &mut Stats, now: Instant) {
        if self.nets.is_empty() {
            return;
        }
        let dt = self
            .if_t
            .map(|t| now.duration_since(t).as_secs_f64())
            .filter(|d| *d > 0.0);
        let mut next = HashMap::new();
        let mut nets = Vec::new();
        let (mut rx, mut tx) = (0.0, 0.0);
        for (name, r, t) in self.nets.drain(..) {
            if next.insert(name.clone(), (r, t)).is_some() || skip_iface(&name, r, t) {
                continue;
            }
            let old = st.nets.iter().find(|n| n.name == name);
            let mut hist = old.map(|n| n.hist.clone()).unwrap_or_default();
            let mut rx_hist = old.map(|n| n.rx_hist.clone()).unwrap_or_default();
            let mut tx_hist = old.map(|n| n.tx_hist.clone()).unwrap_or_default();
            let (mut rr, mut tr) = (0.0, 0.0);
            if let (Some(&(pr, pt)), Some(dt)) = (self.if_prev.get(&name), dt) {
                rr = r.saturating_sub(pr) as f64 / dt;
                tr = t.saturating_sub(pt) as f64 / dt;
                push(&mut hist, rr + tr);
                push(&mut rx_hist, rr);
                push(&mut tx_hist, tr);
            }
            rx += rr;
            tx += tr;
            nets.push(Net {
                name,
                rx: rr,
                tx: tr,
                hist,
                rx_hist,
                tx_hist,
            });
        }
        if dt.is_some() {
            st.net_rx = rx;
            st.net_tx = tx;
            push(&mut st.net_hist, ((rx + tx) / 125_000_000.0).min(1.0));
            push(&mut st.net_rx_hist, rx);
            push(&mut st.net_tx_hist, tx);
        }
        st.nets = nets;
        self.if_prev = next;
        self.if_t = Some(now);
    }

    fn disk_io(&mut self, st: &mut Stats, now: Instant) {
        if self.bdev.is_empty() {
            return;
        }
        let dt = self
            .disk_t
            .map(|t| now.duration_since(t).as_secs_f64())
            .filter(|d| *d > 0.0);
        let real = self.bdev.iter().any(|d| !d.0.starts_with("dm-"));
        let mut next = HashMap::new();
        let mut out = Vec::new();
        for (name, r, w, ticks) in self.bdev.drain(..) {
            next.insert(name.clone(), (r, w, ticks));
            if (real && name.starts_with("dm-")) || (r == 0 && w == 0) {
                continue;
            }
            let old = st.disk_io.iter().find(|d| d.name == name);
            let mut d = DiskIo {
                hist: old.map(|d| d.hist.clone()).unwrap_or_default(),
                read_hist: old.map(|d| d.read_hist.clone()).unwrap_or_default(),
                write_hist: old.map(|d| d.write_hist.clone()).unwrap_or_default(),
                name,
                ..Default::default()
            };
            if let (Some(&(pr, pw, pt)), Some(dt)) = (self.disk_prev.get(&d.name), dt) {
                d.read = r.saturating_sub(pr) as f64 * 512.0 / dt;
                d.write = w.saturating_sub(pw) as f64 * 512.0 / dt;
                // md arrays don't account io time
                if let (Some(t), Some(p)) = (ticks, pt) {
                    if !d.name.starts_with("md") {
                        d.busy = Some((t.saturating_sub(p) as f64 / (dt * 1000.0)).clamp(0.0, 1.0));
                    }
                }
                push(&mut d.hist, d.read + d.write);
                push(&mut d.read_hist, d.read);
                push(&mut d.write_hist, d.write);
            }
            out.push(d);
        }
        st.disk_io = out;
        self.disk_prev = next;
        self.disk_t = Some(now);
    }

    /// Per-pid CPU from CPU-time deltas (like htop), the process table, counts,
    /// and agent processes.
    fn processes(&mut self, st: &mut Stats, now: Instant) {
        if self.procs.is_empty() {
            return;
        }
        let dt = self
            .proc_t
            .map(|t| now.duration_since(t).as_secs_f64())
            .filter(|d| *d > 0.0);
        let raw = std::mem::take(&mut self.procs);
        let mut next: HashMap<String, f64> = HashMap::with_capacity(raw.len());
        let mut all: Vec<Proc> = Vec::with_capacity(raw.len());
        // the rest: idle kernel threads and the like, kept for the tree's ancestors
        let mut idle: HashMap<String, Proc> = HashMap::new();
        let mut pinfo: HashMap<String, PInfo> = HashMap::with_capacity(raw.len());
        let mut agents_in: Vec<(String, String, f64, &'static str)> = Vec::new();
        let (mut running, mut threads) = (0usize, 0usize);
        st.procs_total = raw.len();
        // every process's parent and name: is an agent one of herdr's panes?
        let mut parents: HashMap<String, (String, String)> = HashMap::with_capacity(raw.len());
        for r in &raw {
            parents.insert(r.pid.clone(), (r.ppid.clone(), r.name.clone()));
        }
        for r in raw {
            let (user, cmd) = match self.users.get(&r.pid) {
                Some((u, c)) => (
                    if r.user.is_empty() { u.clone() } else { r.user },
                    c.clone(),
                ),
                None => (r.user, String::new()),
            };
            let cpu = match (r.secs, r.secs.and(self.proc_prev.get(&r.pid)), dt) {
                (Some(s), Some(p), Some(dt)) => (s - p).max(0.0) / dt,
                _ => r.pcpu.unwrap_or(0.0),
            };
            if let Some(s) = r.secs {
                next.insert(r.pid.clone(), s);
            }
            let thr = if r.threads > 0 {
                r.threads
            } else {
                self.threads.get(&r.pid).copied().unwrap_or(0)
            };
            threads += thr as usize;
            running += usize::from(r.state == "R");
            if let Some(a) = agent_of(&r.name, &cmd) {
                agents_in.push((r.pid.clone(), r.ppid.clone(), r.rss, a));
            }
            let time = r.secs.unwrap_or(0.0);
            let avg_cpu = r
                .age
                .filter(|a| *a > 0 && r.secs.is_some())
                .map(|a| time / a as f64);
            pinfo.insert(
                r.pid.clone(),
                PInfo {
                    cpu,
                    avg_cpu,
                    age: r.age,
                    user: user.clone(),
                    cmd: cmd.clone(),
                    name: r.name.clone(),
                    ppid: r.ppid.clone(),
                },
            );
            let p = Proc {
                pid: r.pid,
                user,
                cpu,
                rss: r.rss,
                name: r.name,
                state: r.state,
                threads: thr,
                cmd,
                time,
                ppid: r.ppid,
                age: r.age,
                avg_cpu,
            };
            if cpu > 0.005 || p.rss > 0.0 {
                all.push(p);
            } else {
                idle.insert(p.pid.clone(), p);
            }
        }
        // loadavg's "running/total" counts threads: a fallback where ps can't
        let (lr, lt) = st.runnable.split_once('/').unwrap_or(("", ""));
        st.procs_running = if running > 0 {
            running
        } else {
            lr.parse().unwrap_or(0)
        };
        st.threads_total = if threads > 0 {
            threads
        } else {
            lt.parse().unwrap_or(0)
        };
        all.sort_by(|a, b| by_num(a.cpu, b.cpu).then(by_num(a.rss, b.rss)));
        let cut = all.split_off(all.len().min(PROC_ROWS));
        // every kept process's ancestors stay too, so the tree has no holes
        let mut have: std::collections::HashSet<String> =
            all.iter().map(|p| p.pid.clone()).collect();
        let mut pool: HashMap<String, Proc> = cut.into_iter().map(|p| (p.pid.clone(), p)).collect();
        pool.extend(idle);
        let mut i = 0;
        while i < all.len() {
            let pp = all[i].ppid.clone();
            if !have.contains(&pp) {
                if let Some(p) = pool.remove(&pp) {
                    have.insert(pp);
                    all.push(p);
                }
            }
            i += 1;
        }
        st.agent_list = agent_list(&agents_in, &parents, &pinfo);
        self.pinfo = pinfo;
        st.top = all.iter().take(TOP_ROWS).cloned().collect();
        st.procs = all;
        st.agent_procs = agent_table(&agents_in);
        let claude = st.agent_procs.iter().find(|a| a.0 == "claude");
        st.claude_procs = claude.map_or(0, |a| a.1);
        st.claude_rss = claude.map_or(0.0, |a| a.2);
        self.proc_prev = next;
        self.proc_t = Some(now);
    }

    fn gpu(&mut self, st: &mut Stats) {
        let note = self.gpu_note.take();
        if !self.rocm.is_empty() {
            let lines = std::mem::take(&mut self.rocm);
            self.gpus = parse_rocm(&lines);
        }
        if std::mem::take(&mut self.gpu_seen) {
            st.has_gpu_tool = true;
            let mut g = std::mem::take(&mut self.gpus);
            for x in g.iter_mut() {
                if let Some(o) = st
                    .gpus
                    .iter()
                    .find(|o| o.index == x.index && o.vendor == x.vendor)
                {
                    x.hist = o.hist.clone();
                    x.mem_hist = o.mem_hist.clone();
                    x.power_hist = o.power_hist.clone();
                    x.temp_hist = o.temp_hist.clone();
                }
                push(&mut x.hist, x.util);
                if let Some(t) = x.temp {
                    push(&mut x.temp_hist, t);
                }
                if x.mem_total > 0.0 {
                    push(&mut x.mem_hist, (x.mem_used / x.mem_total).clamp(0.0, 1.0));
                }
                if let (Some(p), Some(l)) = (x.power, x.power_limit.filter(|l| *l > 0.0)) {
                    push(&mut x.power_hist, (p / l).clamp(0.0, 1.0));
                }
            }
            st.gpu_note = match note {
                Some(n) => Some(n),
                None if g.is_empty() => Some("the GPU tool found no GPUs".into()),
                None => None,
            };
            st.gpus = g;
        } else if note.is_some() {
            st.gpu_note = note;
        }
        self.gpus.clear();
        if let Some(mut p) = self.gpu_procs.take() {
            for x in p.iter_mut() {
                if let Some(g) = st.gpus.iter().find(|g| {
                    g.bus.eq_ignore_ascii_case(&x.gpu)
                        || g.bus.ends_with(&x.gpu)
                        || x.gpu.ends_with(&g.bus)
                }) {
                    x.gpu = g.index.clone();
                }
                if x.user.is_empty() {
                    if let Some((u, _)) = self.users.get(&x.pid) {
                        x.user = u.clone();
                    }
                }
            }
            p.sort_by(|a, b| by_num(a.mem, b.mem));
            st.gpu_procs = p;
        }
        if std::mem::take(&mut self.pmon_done) {
            self.merge_pmon(st);
        }
        // CPU, age and command line from this sample's process table; the
        // agent among its ancestors, when an agent started it
        for x in st.gpu_procs.iter_mut() {
            x.agent = agent_above(&self.pinfo, &x.pid);
            if let Some(i) = self.pinfo.get(&x.pid) {
                x.cpu = Some(i.cpu);
                x.avg_cpu = i.avg_cpu;
                x.age = i.age;
                if x.cmd.is_empty() {
                    x.cmd = i.cmd.clone();
                }
                if x.user.is_empty() {
                    x.user = i.user.clone();
                }
                // pmon cuts names at 15 characters (`chrome --type=g`): the
                // program from the command line (or the kernel's name) instead
                if x.kind == "G" || x.name.contains(' ') {
                    let argv0 = base(i.cmd.split_whitespace().next().unwrap_or(""));
                    if !argv0.is_empty() && !argv0.starts_with('[') {
                        x.name = argv0.to_string();
                    } else if !i.name.is_empty() {
                        x.name = i.name.clone();
                    }
                }
            }
        }
    }

    /// pmon's per-process utilisation onto the GPU process list; graphics
    /// processes (which `--query-compute-apps` leaves out) join it.
    fn merge_pmon(&mut self, st: &mut Stats) {
        let rows = std::mem::take(&mut self.pmon);
        let mut seen = std::collections::HashSet::new();
        for r in rows {
            // a process idle over the interval reads `-`: 0 for its average
            let e = self.sm_acc.entry(r.pid.clone()).or_insert((0.0, 0));
            e.0 += r.sm.unwrap_or(0.0);
            e.1 += 1;
            let avg = e.0 / e.1 as f64;
            seen.insert(r.pid.clone());
            let gpu = st
                .gpus
                .iter()
                .find(|g| g.index == r.gpu)
                .map(|g| g.index.clone())
                .unwrap_or(r.gpu.clone());
            let x = match st
                .gpu_procs
                .iter_mut()
                .position(|p| p.pid == r.pid && p.gpu == gpu)
            {
                Some(i) => &mut st.gpu_procs[i],
                None => {
                    st.gpu_procs.push(GpuProc {
                        gpu,
                        pid: r.pid.clone(),
                        mem: r.fb.unwrap_or(0.0) * 1048576.0,
                        name: r.name.clone(),
                        ..Default::default()
                    });
                    st.gpu_procs.last_mut().unwrap()
                }
            };
            x.kind = r.kind;
            x.sm = r.sm;
            x.mem_util = r.mem;
            x.enc = r.enc;
            x.dec = r.dec;
            x.sm_avg = Some(avg);
            if x.mem <= 0.0 {
                x.mem = r.fb.unwrap_or(0.0) * 1048576.0;
            }
            if x.name.is_empty() {
                x.name = r.name;
            }
        }
        self.sm_acc.retain(|p, _| seen.contains(p));
        st.gpu_procs.sort_by(|a, b| by_num(a.mem, b.mem));
    }
}

/// A running sampler for one host. Dropping it kills the ssh stream.
impl Stats {
    /// Every history it keeps, by a name that holds from one sample to the next.
    pub fn hists_mut(&mut self) -> Vec<(String, &mut Vec<f64>)> {
        let mut v: Vec<(String, &mut Vec<f64>)> = vec![
            ("cpu".into(), &mut self.cpu_hist),
            ("cpu_user".into(), &mut self.cpu_user_hist),
            ("cpu_system".into(), &mut self.cpu_system_hist),
            ("cpu_iowait".into(), &mut self.cpu_iowait_hist),
            ("cpu_temp".into(), &mut self.cpu_temp_hist),
            ("load".into(), &mut self.load_hist),
            ("mem".into(), &mut self.mem_hist),
            ("mem_cache".into(), &mut self.mem_cache_hist),
            ("swap".into(), &mut self.swap_hist),
            ("net".into(), &mut self.net_hist),
            ("net_rx".into(), &mut self.net_rx_hist),
            ("net_tx".into(), &mut self.net_tx_hist),
        ];
        for g in &mut self.gpus {
            let k = if g.bus.is_empty() { &g.index } else { &g.bus };
            let k = format!("gpu:{k}");
            v.push((format!("{k}:util"), &mut g.hist));
            v.push((format!("{k}:mem"), &mut g.mem_hist));
            v.push((format!("{k}:power"), &mut g.power_hist));
            v.push((format!("{k}:temp"), &mut g.temp_hist));
        }
        for n in &mut self.nets {
            v.push((format!("if:{}", n.name), &mut n.hist));
            v.push((format!("if:{}:rx", n.name), &mut n.rx_hist));
            v.push((format!("if:{}:tx", n.name), &mut n.tx_hist));
        }
        for d in &mut self.disk_io {
            v.push((format!("disk:{}", d.name), &mut d.hist));
            v.push((format!("disk:{}:read", d.name), &mut d.read_hist));
            v.push((format!("disk:{}:write", d.name), &mut d.write_hist));
        }
        v
    }
}

pub struct Telemetry {
    pub alias: String,
    pub stats: Arc<Mutex<Stats>>,
    child: Arc<Mutex<Option<Child>>>,
    stop: Arc<Mutex<bool>>,
}

impl Telemetry {
    pub fn start(alias: &str, interval: u64) -> Telemetry {
        let stats = Arc::new(Mutex::new(Stats::default()));
        let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let stop = Arc::new(Mutex::new(false));
        let (a, st, ch, sp) = (
            alias.to_string(),
            stats.clone(),
            child.clone(),
            stop.clone(),
        );
        thread::spawn(move || {
            let mut backoff = 2u64;
            loop {
                if *sp.lock().unwrap() {
                    return;
                }
                let r = run_once(&a, interval.max(1), &st, &ch, &sp);
                if *sp.lock().unwrap() {
                    return;
                }
                st.lock().unwrap().err = Some(r.unwrap_or_else(|| "stats stream ended".into()));
                thread::sleep(Duration::from_secs(backoff));
                backoff = (backoff * 2).min(30);
            }
        });
        Telemetry {
            alias: alias.to_string(),
            stats,
            child,
            stop,
        }
    }

    pub fn snapshot(&self) -> Stats {
        self.stats.lock().unwrap().clone()
    }

    /// A sampler that never runs, holding `st` (render tests).
    #[cfg(test)]
    pub fn fixed(alias: &str, st: Stats) -> Telemetry {
        Telemetry {
            alias: alias.to_string(),
            stats: Arc::new(Mutex::new(st)),
            child: Arc::new(Mutex::new(None)),
            stop: Arc::new(Mutex::new(true)),
        }
    }

    /// Just the GPU processes (cheaper than a whole snapshot).
    pub fn gpu_procs(&self) -> Vec<GpuProc> {
        self.stats.lock().unwrap().gpu_procs.clone()
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        *self.stop.lock().unwrap() = true;
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn run_once(
    alias: &str,
    interval: u64,
    st: &Arc<Mutex<Stats>>,
    ch: &Arc<Mutex<Option<Child>>>,
    stop: &Arc<Mutex<bool>>,
) -> Option<String> {
    // showcase's demo world: its stats, until stopped
    if crate::showcase::stats(alias, interval, st, stop) {
        return None;
    }
    let mut cmd = sshcfg::shell(alias, "sh -s");
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Some(format!("ssh: {e}")),
    };
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(script(interval).as_bytes());
        // dropping stdin: sh has the whole loop and keeps running
    }
    let out = child.stdout.take()?;
    let err = child.stderr.take();
    {
        let mut g = ch.lock().unwrap();
        if *stop.lock().unwrap() {
            let _ = child.kill();
            return None;
        }
        *g = Some(child);
    }
    let errbuf = Arc::new(Mutex::new(String::new()));
    if let Some(e) = err {
        let eb = errbuf.clone();
        thread::spawn(move || {
            for l in BufReader::new(e).lines().map_while(Result::ok) {
                let mut g = eb.lock().unwrap();
                if g.len() < 4000 {
                    g.push_str(&l);
                    g.push('\n');
                }
            }
        });
    }
    let mut acc = Acc::default();
    for l in BufReader::new(out).lines().map_while(Result::ok) {
        let mut g = st.lock().unwrap();
        let before = g.samples;
        acc.line(&mut g, &l);
        if g.samples != before {
            crate::statshist::record(alias, crate::cache::now(), &mut g);
        }
    }
    if let Some(mut c) = ch.lock().unwrap().take() {
        let _ = c.wait();
    }
    let e = errbuf.lock().unwrap().clone();
    Some(
        e.lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| "stats stream ended".into()),
    )
}

// ------------------------------------------------------------------ drawing

/// `CPU 34% · GPU 93% · MEM 61%` — for machine group headers.
pub fn summary(st: &Stats) -> Vec<Span<'static>> {
    if !st.live() {
        return vec![];
    }
    let mut v = Vec::new();
    let mut item = |k: &str, frac: f64| {
        if !v.is_empty() {
            v.push(Span::styled("  ", ui::faint()));
        }
        v.push(Span::styled(format!("{k} "), ui::faint()));
        v.push(Span::styled(
            format!("{:>3.0}%", frac * 100.0),
            Style::new().fg(ui::heat(frac)),
        ));
    };
    item("cpu", st.cpu);
    if let Some(g) = st.gpu_util() {
        item("gpu", g);
    }
    item("mem", st.mem_frac());
    v
}

fn label(s: &str) -> Span<'static> {
    Span::styled(
        format!("{s:<5}"),
        ui::fg(c::MUTED()).add_modifier(Modifier::BOLD),
    )
}

/// The machine panel (htop × nvidia-smi). Fits whatever rect it's given:
/// sections are added in priority order until the height runs out.
pub fn draw(f: &mut Frame, r: Rect, st: &Stats, color: Color, alias: &str, t0: Instant) {
    let title = Line::from(vec![
        Span::styled("▌", Style::new().fg(color)),
        Span::styled(
            format!("{alias} "),
            Style::new().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if st.hostname.is_empty() || st.hostname == alias {
                String::new()
            } else {
                format!("{} ", st.hostname)
            },
            ui::muted(),
        ),
    ]);
    let mut block = ui::panel(title, false);
    if st.uptime > 0 {
        block = block.title_top(
            Line::from(Span::styled(
                format!(" up {} ", ui::dur(st.uptime)),
                ui::faint(),
            ))
            .right_aligned(),
        );
    }
    let foot = facts(st);
    if !foot.spans.is_empty() {
        block = block.title_bottom(foot);
    }
    let inner = block.inner(r);
    f.render_widget(block, r);
    let w = inner.width as usize;
    let h = inner.height as usize;
    if w < 20 || h == 0 {
        return;
    }
    if st.samples == 0 {
        let msg = match &st.err {
            Some(e) => Line::from(vec![
                Span::styled("✕ ", ui::fg(c::RED())),
                Span::styled(
                    format!("no stats: {}", ui::trunc(e, w.saturating_sub(12))),
                    ui::muted(),
                ),
            ]),
            None => Line::from(vec![
                Span::styled(format!("{} ", ui::spinner(t0)), ui::fg(c::ACCENT())),
                Span::styled("sampling…", ui::muted()),
            ]),
        };
        f.render_widget(Paragraph::new(msg), inner);
        return;
    }
    let mut lines: Vec<Line> = Vec::new();
    let bw = (w.saturating_sub(5 + 6 + 2)).clamp(6, 40);
    let spark_w = w.saturating_sub(5 + bw + 6 + 2);

    // CPU: bar + % + sparkline
    let mut l = vec![label("cpu")];
    l.extend(ui::bar(st.cpu, bw, ui::heat(st.cpu)));
    l.push(Span::styled(
        format!(" {:>3.0}% ", st.cpu * 100.0),
        ui::bold(),
    ));
    if spark_w >= 6 {
        l.push(ui::spark(&st.cpu_hist, spark_w, c::TEAL()));
    }
    lines.push(Line::from(l));
    // per-core strip (wraps), htop style
    if !st.cores.is_empty() {
        let room = w.saturating_sub(5);
        for chunk in st.cores.chunks(room.max(1)).take(2) {
            let mut l = vec![Span::styled("     ", Style::new())];
            l.extend(chunk.iter().map(|v| ui::level(*v)));
            lines.push(Line::from(l));
        }
    }
    let mut l = vec![Span::styled("     ", Style::new())];
    l.push(Span::styled(
        format!(
            "load {:.2} {:.2} {:.2}{}",
            st.load[0],
            st.load[1],
            st.load[2],
            if st.ncpu > 0 {
                format!(" · {}×", st.ncpu)
            } else {
                String::new()
            }
        ),
        ui::faint(),
    ));
    if !st.cpu_model.is_empty() {
        l.push(Span::styled(
            format!(" {}", ui::trunc(&st.cpu_model, w.saturating_sub(30))),
            ui::faint(),
        ));
    }
    lines.push(Line::from(l));

    // memory
    if st.mem_total > 0.0 {
        let fr = st.mem_frac();
        let mut l = vec![label("mem")];
        l.extend(ui::bar(fr, bw, ui::heat(fr)));
        l.push(Span::styled(format!(" {:>3.0}% ", fr * 100.0), ui::bold()));
        l.push(Span::styled(
            format!("{}/{}", ui::bytes(st.mem_used), ui::bytes(st.mem_total)),
            ui::muted(),
        ));
        lines.push(Line::from(l));
        if st.swap_total > 0.0 && st.swap_used > 1e6 {
            lines.push(Line::from(vec![
                Span::styled("     ", Style::new()),
                Span::styled(
                    format!(
                        "swap {}/{}",
                        ui::bytes(st.swap_used),
                        ui::bytes(st.swap_total)
                    ),
                    ui::faint(),
                ),
            ]));
        }
    }

    // GPUs
    for g in &st.gpus {
        let fr = g.util;
        let mf = if g.mem_total > 0.0 {
            g.mem_used / g.mem_total
        } else {
            0.0
        };
        let gb = bw.min(18);
        let mut l = vec![Span::styled(
            format!("gpu{:<2}", g.index),
            ui::fg(c::MUTED()).add_modifier(Modifier::BOLD),
        )];
        l.extend(ui::bar(fr, gb, ui::heat(fr)));
        l.push(Span::styled(format!(" {:>3.0}% ", fr * 100.0), ui::bold()));
        let mut extra = format!("{}/{}", ui::bytes(g.mem_used), ui::bytes(g.mem_total));
        if let Some(t) = g.temp {
            extra.push_str(&format!(" {t:.0}°"));
        }
        if let (Some(p), Some(pl)) = (g.power, g.power_limit) {
            extra.push_str(&format!(" {p:.0}/{pl:.0}W"));
        }
        l.push(Span::styled(extra, Style::new().fg(ui::heat(mf))));
        let used: usize = l.iter().map(|s| ui::width(&s.content)).sum();
        if w > used + 8 {
            l.push(Span::raw(" "));
            l.push(ui::spark(&g.hist, w - used - 1, c::MAUVE()));
        }
        lines.push(Line::from(l));
        if w >= 50 {
            lines.push(Line::from(vec![
                Span::styled("     ", Style::new()),
                Span::styled(ui::trunc(&g.name, w.saturating_sub(5)), ui::faint()),
            ]));
        }
    }
    if !st.gpu_procs.is_empty() {
        for p in st.gpu_procs.iter().take(3) {
            lines.push(Line::from(vec![
                Span::styled("     ", Style::new()),
                Span::styled(format!("↳ gpu{} ", p.gpu), ui::faint()),
                Span::styled(ui::pad(&p.name, 14), Style::new()),
                Span::styled(format!(" {:>7} ", ui::bytes(p.mem)), ui::fg(c::MAUVE())),
                Span::styled(p.user.clone(), ui::faint()),
            ]));
        }
    }

    // network + disks
    if st.net_rx + st.net_tx > 0.0 || !st.disks.is_empty() {
        let mut l = vec![label("io")];
        if st.net_rx + st.net_tx > 0.0 {
            l.push(Span::styled(
                format!("↓{}/s ", ui::bytes(st.net_rx)),
                ui::fg(c::GREEN()),
            ));
            l.push(Span::styled(
                format!("↑{}/s", ui::bytes(st.net_tx)),
                ui::fg(c::BLUE()),
            ));
        }
        for (m, t, u) in &st.disks {
            if *t <= 0.0 {
                continue;
            }
            let fr = u / t;
            l.push(Span::styled(
                format!("  {} ", if m == "/" { "/" } else { "~" }),
                ui::faint(),
            ));
            l.push(Span::styled(
                format!("{:.0}%", fr * 100.0),
                Style::new().fg(ui::heat(fr)),
            ));
        }
        lines.push(Line::from(l));
    }

    // top processes fill what's left
    let left = h.saturating_sub(lines.len());
    if left >= 3 && !st.top.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(
                "{:<5}{:<16}{:>6}{:>8}  user",
                "top", "process", "cpu", "mem"
            ),
            ui::faint(),
        )));
        for p in st.top.iter().take(left - 1) {
            let hot = p.cpu / st.ncpu.max(1) as f64;
            lines.push(Line::from(vec![
                Span::styled("     ", Style::new()),
                Span::styled(
                    ui::pad(&p.name, 15),
                    if crate::agents::by_process(&p.name).is_some() {
                        ui::fg(c::ACCENT())
                    } else {
                        Style::new()
                    },
                ),
                Span::styled(
                    format!(" {:>5.0}%", p.cpu * 100.0),
                    Style::new().fg(ui::heat(hot.max(p.cpu.min(1.0) * 0.5))),
                ),
                Span::styled(format!("{:>8}", ui::bytes(p.rss)), ui::muted()),
                Span::styled(format!("  {}", ui::trunc(&p.user, 10)), ui::faint()),
            ]));
        }
    }
    lines.truncate(h);
    f.render_widget(Paragraph::new(lines), inner);
}

/// Fun facts for the panel's bottom border.
fn facts(st: &Stats) -> Line<'static> {
    let mut v: Vec<Span> = Vec::new();
    // every agent running there: `✻ claude 2 · codex 1 · 1.3G`
    if !st.agent_procs.is_empty() {
        v.push(Span::styled(" ✻ ", ui::fg(c::ACCENT())));
        let names: Vec<String> = st
            .agent_procs
            .iter()
            .take(3)
            .map(|(a, n, _)| format!("{a} {n}"))
            .collect();
        let rss: f64 = st.agent_procs.iter().map(|x| x.2).sum();
        v.push(Span::styled(
            format!("{} · {}", names.join(" · "), ui::bytes(rss)),
            ui::muted(),
        ));
    }
    let today: usize = st.sessions_today.iter().map(|x| x.1).sum();
    if today > 0 {
        v.push(Span::styled(
            format!(" · {today} sessions today"),
            ui::muted(),
        ));
    }
    if !st.os.is_empty() {
        v.push(Span::styled(
            format!(" · {} ", ui::trunc(&st.os, 22)),
            ui::faint(),
        ));
    } else if !v.is_empty() {
        v.push(Span::raw(" "));
    }
    Line::from(v)
}

/// The sampler limited to `n` samples one second apart.
fn bounded_script(n: usize) -> String {
    let head = format!("for _r in {}; do", vec!["x"; n.max(1)].join(" "));
    script(1).replace("while :; do", &head)
}

/// Run the bounded sampler through local `sh`; its stdout.
fn run_local(n: usize, path: Option<&str>) -> String {
    let mut c = Command::new("/bin/sh");
    c.arg("-c").arg(bounded_script(n)).stdin(Stdio::null());
    if let Some(p) = path {
        c.env("PATH", p);
    }
    c.output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// For tests and `aleph stats`: two samples of this machine.
#[allow(dead_code)]
pub fn sample_local() -> Stats {
    let mut st = Stats::default();
    let mut acc = Acc::default();
    for l in run_local(2, None).lines() {
        acc.line(&mut st, l);
    }
    st
}

#[cfg(test)]
mod tests {
    use super::*;

    const B31: &str = include_str!("../tests/fixtures/telemetry/b31.txt");
    const MACOS: &str = include_str!("../tests/fixtures/telemetry/macos.txt");

    /// Feed recorded text, every `E` one second after the last.
    fn feed(acc: &mut Acc, st: &mut Stats, text: &str, t0: Instant) -> Instant {
        let mut t = t0;
        for l in text.lines() {
            if l.starts_with('E') {
                t += Duration::from_secs(1);
            }
            acc.line_at(st, l, t);
        }
        t
    }

    fn parse(text: &str) -> Stats {
        let mut st = Stats::default();
        feed(&mut Acc::default(), &mut st, text, Instant::now());
        st
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6 * b.abs().max(1.0)
    }

    #[test]
    fn cpu_names_and_sockets() {
        assert_eq!(
            cpu_name("INTEL(R) XEON(R) GOLD 6544Y"),
            "Intel Xeon Gold 6544Y"
        );
        assert_eq!(
            cpu_name("AMD EPYC 7763 64-Core Processor"),
            "AMD EPYC 7763 64-Core Processor"
        );
        assert_eq!(
            cpu_name("AMD EPYC 9654 96-CORE PROCESSOR"),
            "AMD EPYC 9654 96-CORE Processor"
        );
        assert_eq!(
            cpu_name("Intel(R) Core(TM) i9-12900K CPU @ 3.20GHz"),
            "Intel Core i9-12900K"
        );
        // q7x: two packages, 64 threads
        let st = parse("C\t64\tINTEL(R) XEON(R) GOLD 6544Y\t100\t4096\t2\n");
        assert_eq!((st.ncpu, st.sockets), (64, 2));
        assert_eq!(st.cpu_model, "Intel Xeon Gold 6544Y");
        // an older stream without the field
        assert_eq!(parse("C\t8\tApple M2\t100\t16384\n").sockets, 0);
    }

    /// Run one of the script's awk programs on files, or on `stdin`.
    fn awk(prog: &str, files: &[std::path::PathBuf], stdin: &str) -> String {
        let mut c = Command::new("awk")
            .arg(prog)
            .args(files)
            .env("LC_ALL", "C")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("awk");
        c.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        let o = c.wait_with_output().unwrap();
        String::from_utf8(o.stdout).unwrap()
    }

    /// A /proc/PID/stat line with the fields we read filled in.
    #[allow(clippy::too_many_arguments)]
    fn stat_line(
        pid: u32,
        comm: &str,
        state: &str,
        ppid: u32,
        ut: u64,
        stime: u64,
        thr: u32,
        rss: u64,
    ) -> String {
        format!(
            "{pid} ({comm}) {state} {ppid} 1 1 0 -1 4194560 100 0 0 0 {ut} {stime} 0 0 20 0 {thr} 0 12345 \
             1000000 {rss} 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0"
        )
    }

    #[test]
    fn parses_a_canned_sample() {
        let mut st = Stats::default();
        let mut acc = Acc::default();
        let a = "H\tbox\tLinux 6.8\tUbuntu 22.04\nC\t4\tAMD Ryzen 9 7950X 16-Core Processor\n\
S\tcpu 100 0 100 800 0 0 0 0\nS\tcpu0 25 0 25 200 0 0 0 0\nS\tcpu1 25 0 25 200 0 0 0 0\n\
M\tMemTotal:\t1000\nM\tMemAvailable:\t250\nL\t1.5 1.0 0.5\t2/300\nU\t90061\nN\teth0\t1000\t2000\n\
Q\t10\t100\t1000\tpython\nQ\t11\t5\t100\tclaude\n\
G\t0, NVIDIA GeForce RTX 4090, 93, 20000, 24564, 71, 384.2, 450.00, 60, 00000000:01:00.0\ng\n\
A\t00000000:01:00.0\t10\t19000\tselene\t/usr/bin/python\na\nD\t/\t1000\t500\nK\t41\nE\t1\n";
        let b = "S\tcpu 150 0 150 900 0 0 0 0\nS\tcpu0 50 0 50 220 0 0 0 0\nS\tcpu1 25 0 25 280 0 0 0 0\n\
M\tMemTotal:\t1000\nM\tMemAvailable:\t250\nN\teth0\t3000\t2500\nQ\t10\t300\t1000\tpython\nQ\t11\t6\t100\tclaude\n\
G\t0, NVIDIA GeForce RTX 4090, 50, 20000, 24564, 70, 300, 450, 60, 00000000:01:00.0\ng\nE\t2\n";
        let t = feed(&mut acc, &mut st, a, Instant::now());
        feed(&mut acc, &mut st, b, t);
        assert_eq!(st.hostname, "box");
        assert_eq!(st.platform, "linux");
        assert_eq!(st.cpu_model, "AMD Ryzen 9 7950X 16-Core Processor");
        assert!(close(st.cpu, 0.5), "{}", st.cpu); // 100 busy of 200
        assert_eq!(st.cores.len(), 2);
        assert!(close(st.mem_frac(), 0.75));
        assert_eq!(st.load, [1.5, 1.0, 0.5]);
        assert_eq!(st.uptime, 90061);
        assert!(close(st.net_rx, 2000.0) && close(st.net_tx, 500.0));
        assert_eq!(st.gpus.len(), 1);
        assert_eq!(st.gpus[0].name, "RTX 4090");
        assert!(close(st.gpus[0].util, 0.5));
        assert_eq!(st.gpus[0].hist.len(), 2);
        assert_eq!(st.gpus[0].vendor, "nvidia");
        assert_eq!(st.gpu_procs[0].gpu, "0");
        assert_eq!(st.gpu_procs[0].name, "python");
        assert_eq!(st.top[0].name, "python");
        assert!(close(st.top[0].cpu, 2.0)); // 200 ticks in 1 s
        assert_eq!(st.claude_procs, 1);
        assert_eq!(st.convos_today, Some(41));
        assert_eq!(st.disks.len(), 1);
        assert_eq!(st.gpu_note, None);
        assert!(st.live());
    }

    /// The Linux awk programs on fake /proc files, two samples a second apart.
    #[test]
    fn linux_two_samples_from_fake_proc() {
        let dir = std::env::temp_dir().join(format!("ch-telemetry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let names = ["stat", "meminfo", "loadavg", "uptime", "dev", "diskstats"];
        let files: Vec<_> = names.iter().map(|n| dir.join(n)).collect();
        let write = |stat: &str, dev: &str, disk: &str| {
            let meminfo = "MemTotal:       16000000 kB\nMemFree:         2000000 kB\nMemAvailable:    8000000 kB\n\
Buffers:          500000 kB\nCached:          4000000 kB\nSwapCached:        10000 kB\nShmem:            300000 kB\n\
SReclaimable:     700000 kB\nSwapTotal:       2000000 kB\nSwapFree:        1500000 kB\n";
            for (f, s) in files.iter().zip([
                stat,
                meminfo,
                "2.50 1.75 1.00 3/1234 5678\n",
                "12345.67 50000.00\n",
                dev,
                disk,
            ]) {
                std::fs::write(f, s).unwrap();
            }
        };
        let dev = |eth: (u64, u64), wl: (u64, u64)| {
            format!(
                "Inter-|   Receive                                                |  Transmit\n \
face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    \
lo: 5000 10 0 0 0 0 0 0 5000 10 0 0 0 0 0 0\n  eth0: {} 100 0 0 0 0 0 0 {} 200 0 0 0 0 0 0\n\
wlp0s20f3:{} 201760561    0 698262    0     0          0         0 {} 89123069    0  259    0     0       0          0\n\
veth123: 100 1 0 0 0 0 0 0 100 1 0 0 0 0 0 0\n  eno2:       0       0    0    0    0     0          0         0        0       0    0    0    0     0       0          0\n",
                eth.0, eth.1, wl.0, wl.1
            )
        };
        let disk = |r: u64, w: u64, io: u64| {
            format!(
                "   7       0 loop0 100 0 200 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n \
259       0 nvme0n1 1000 0 {r} 500 2000 0 {w} 800 0 {io} 1300 0 0 0 0 0 0\n \
259       1 nvme0n1p1 900 0 18000 400 1900 0 38000 700 0 2900 1100 0 0 0 0 0 0\n   \
8       0 sda 10 0 80 5 0 0 0 0 0 20 5 0 0 0 0 0 0\n 253       0 dm-0 50 0 400 10 60 0 480 20 0 100 30 0 0 0 0 0 0\n   \
9       0 md0 5 0 40 0 5 0 40 0 0 0 0 0 0 0 0 0 0\n"
            )
        };
        let procs = |claude: u64, evil: u64| {
            [
                stat_line(1, "systemd", "S", 0, 100, 50, 1, 3000),
                // a kernel thread (no RSS) that used CPU: 10 ticks in the second
                stat_line(4242, "UVM global queue", "S", 2, 0, 20 + evil / 5, 1, 0),
                stat_line(4243, "evil) R 9 (name", "R", 1, evil, 0, 3, 10),
                stat_line(5000, "node", "S", 4999, 10, 10, 11, 50000),
                stat_line(5001, "claude", "S", 1, claude, 100, 20, 100000),
                stat_line(5002, "claude", "S", 5001, 5, 5, 4, 20000),
                stat_line(6000, "kworker/0:1", "I", 2, 0, 0, 1, 0),
            ]
            .join("\n")
        };
        let temps = "/sys/class/hwmon/hwmon0/name:acpitz\n/sys/class/hwmon/hwmon0/temp1_input:27800\n\
/sys/class/hwmon/hwmon2/name:nvme\n/sys/class/hwmon/hwmon2/temp1_label:Composite\n/sys/class/hwmon/hwmon2/temp1_input:45850\n\
/sys/class/hwmon/hwmon2/temp2_label:Sensor 1\n/sys/class/hwmon/hwmon2/temp2_input:45850\n\
/sys/class/hwmon/hwmon4/name:coretemp\n/sys/class/hwmon/hwmon4/temp1_label:Package id 0\n/sys/class/hwmon/hwmon4/temp1_input:61000\n\
/sys/class/hwmon/hwmon4/temp2_label:Core 0\n/sys/class/hwmon/hwmon4/temp2_input:58000\n\
/sys/class/hwmon/hwmon4/temp3_label:Core 1\n/sys/class/hwmon/hwmon4/temp3_input:63000\n\
/sys/class/hwmon/hwmon5/device/name:it8728\n/sys/class/hwmon/hwmon5/device/temp1_input:127000\n\
/sys/class/hwmon/hwmon5/device/temp2_input:35000\n\
/sys/class/thermal/thermal_zone0/type:x86_pkg_temp\n/sys/class/thermal/thermal_zone0/temp:61000\n";

        write(
            "cpu  1000 100 500 8000 200 10 40 50 0 0\ncpu0 500 50 250 4000 100 5 20 25 0 0\n\
cpu1 500 50 250 4000 100 5 20 25 0 0\nintr 12345 0 0\nctxt 999\n",
            &dev((1_000_000, 2_000_000), (255_786_544_277, 20_111_052_576)),
            &disk(20000, 40000, 3000),
        );
        let mut a = String::from("H\tbox\tLinux 6.8.0\tUbuntu 24.04\nC\t2\tIntel(R) Xeon(R) CPU E5-2680 v4 @ 2.40GHz\t100\t4096\n");
        a += &awk(SYS_AWK, &files, "");
        a += &awk(STAT_AWK, &[], &procs(1000, 0));
        a +=
            "W\t5000\tselene\tnode /usr/lib/node_modules/@google/gemini-cli/dist/index.js --yolo\n\
W\t5001\tselene\tclaude --resume\nW\t4243\troot\t[evil]\n";
        a += &awk(TEMP_AWK, &[], temps);
        a += "E\t1\n";
        write(
            "cpu  1300 200 650 9000 400 20 80 250 0 0\ncpu0 650 100 325 4500 200 10 40 125 0 0\n\
cpu1 650 100 325 4500 200 10 40 125 0 0\nintr 12345 0 0\n",
            &dev((2_000_000, 2_500_000), (255_786_546_277, 20_111_053_576)),
            &disk(22048, 44096, 3250),
        );
        let mut b = awk(SYS_AWK, &files, "");
        b += &awk(STAT_AWK, &[], &procs(1100, 50));
        b += "E\t2\n";
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            a.contains("Q\t4243\t0+0\t10\tevil) R 9 (name\tR\t3\t1\t12345\n"),
            "{a}"
        );
        assert!(!a.contains("loop0") && !a.contains("nvme0n1p1") && !a.contains("intr"));
        let mut st = Stats::default();
        let mut acc = Acc::default();
        let t = feed(&mut acc, &mut st, &a, Instant::now());
        feed(&mut acc, &mut st, &b, t);

        // CPU: Δ user 300 nice 100 system 150 idle 1000 iowait 200 irq 10 softirq 40 steal 200
        assert!(close(st.cpu, 0.4), "{}", st.cpu);
        assert!(close(st.cpu_user, 0.2) && close(st.cpu_system, 0.1));
        assert!(close(st.cpu_iowait, 0.1) && close(st.cpu_steal, 0.1));
        assert_eq!(st.cores.len(), 2);
        assert_eq!(st.cpu_model, "Intel Xeon E5-2680 v4");
        assert_eq!(st.load_hist, vec![2.5, 2.5]);
        // memory
        assert!(close(st.mem_total, 16e6 * 1024.0) && close(st.mem_used, 8e6 * 1024.0));
        assert!(close(st.mem_available, 8e6 * 1024.0));
        assert!(close(st.mem_buffers, 5e5 * 1024.0));
        assert!(close(st.mem_cached, 4.7e6 * 1024.0));
        assert!(close(st.mem_shared, 3e5 * 1024.0));
        assert!(close(st.swap_used, 5e5 * 1024.0));
        assert_eq!(st.swap_hist, vec![0.25, 0.25]);
        assert_eq!(st.uptime, 12345);
        // network: lo, veth and the never-used eno2 left out; long names parse
        let names: Vec<&str> = st.nets.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["eth0", "wlp0s20f3"]);
        assert!(close(st.nets[0].rx, 1e6) && close(st.nets[0].tx, 5e5));
        assert!(close(st.nets[1].rx, 2000.0) && close(st.nets[1].tx, 1000.0));
        assert_eq!(st.nets[0].hist, vec![1.5e6]);
        assert!(close(st.net_rx, 1_002_000.0));
        // disks: whole devices only; dm-N dropped next to real ones; md has no busy
        let names: Vec<&str> = st.disk_io.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["nvme0n1", "sda", "md0"]);
        let nv = &st.disk_io[0];
        assert!(close(nv.read, 1048576.0) && close(nv.write, 2097152.0));
        assert_eq!(nv.busy, Some(0.25));
        assert_eq!(st.disk_io[1].busy, Some(0.0));
        assert_eq!(st.disk_io[2].busy, None);
        // processes
        assert_eq!(st.procs_total, 7);
        assert_eq!(st.procs_running, 1);
        assert_eq!(st.threads_total, 1 + 1 + 3 + 11 + 20 + 4 + 1);
        let p = |pid: &str| st.procs.iter().find(|p| p.pid == pid).cloned();
        let evil = p("4243").expect("odd name");
        assert_eq!(evil.name, "evil) R 9 (name");
        assert_eq!((evil.state.as_str(), evil.threads), ("R", 3));
        assert!(close(evil.cpu, 0.5) && close(evil.time, 0.5));
        assert_eq!(evil.user, "root");
        let cl = p("5001").unwrap();
        assert!(close(cl.cpu, 1.0) && close(cl.time, 12.0) && close(cl.rss, 100000.0 * 4096.0));
        // started 12345 ticks (123.45 s) after a boot 12345 s ago
        assert_eq!(cl.age, Some(12221));
        assert!(close(cl.avg_cpu.unwrap(), 12.0 / 12221.0));
        assert_eq!(p("5002").unwrap().ppid, "5001");
        assert_eq!(
            (cl.cmd.as_str(), cl.user.as_str()),
            ("claude --resume", "selene")
        );
        assert_eq!(st.top[0].pid, "5001");
        assert!(p("6000").is_none(), "idle kernel threads stay out");
        let uvm = p("4242").expect("a busy kernel thread");
        assert_eq!((uvm.name.as_str(), uvm.rss), ("UVM global queue", 0.0));
        assert!(close(uvm.cpu, 0.1));
        // agents: claude's helper child adds memory, not a count; node runs gemini
        assert_eq!(
            st.agent_procs,
            vec![
                ("claude".into(), 1, 120000.0 * 4096.0),
                ("gemini".into(), 1, 50000.0 * 4096.0)
            ]
        );
        assert_eq!((st.claude_procs, st.claude_rss), (1, 120000.0 * 4096.0));
        // temperatures
        assert_eq!(st.cpu_temp, Some(61.0));
        assert_eq!(
            st.temps,
            vec![
                ("cpu".into(), 61.0),
                ("cores max".into(), 63.0),
                ("acpi".into(), 27.8),
                ("nvme0".into(), 45.85),
                ("it8728".into(), 35.0),
            ]
        );
    }

    #[test]
    fn a_gpu_process_knows_the_agent_and_the_session_it_came_from() {
        let mut st = Stats::default();
        let mut acc = Acc::default();
        let gpu = "G\t0, NVIDIA GeForce RTX 4090, 50, 1000, 24564, 44, 26.41, 450.00, 0, 00000000:01:00.0";
        // claude (3000) → its shell (3500) → python (4000) on the GPU; a
        // second python (4100) nobody's agent started
        let text = format!(
            "U\t1000\nC\t24\tIntel\t100\t4096\n\
Q\t3000\t10+0\t100\tclaude\tS\t8\t1\t50000\n\
Q\t3500\t1+0\t10\tzsh\tS\t1\t3000\t50000\n\
Q\t4000\t5000+0\t100\tpython\tR\t8\t3500\t50000\n\
Q\t4100\t5000+0\t100\tpython\tR\t8\t1\t50000\n{gpu}\ng\n\
A\t00000000:01:00.0\t4000\t2048\t\t/usr/bin/python3\n\
A\t00000000:01:00.0\t4100\t1024\t\t/usr/bin/python3\n\
V\t4000\t/srv/px/train\nV\t4100\t/tmp\na\nE\t1\n"
        );
        feed(&mut acc, &mut st, &text, Instant::now());
        let p = st.gpu_procs.iter().find(|p| p.pid == "4000").unwrap();
        assert_eq!((p.agent, p.dir.as_str()), (Some("claude"), "/srv/px/train"));
        let q = st.gpu_procs.iter().find(|p| p.pid == "4100").unwrap();
        assert_eq!(q.agent, None);
        let sess = |name: &str, agent: &str, cwd: &str| crate::herdr::Session {
            host: "b31".into(),
            name: name.into(),
            agent: agent.into(),
            cwd: cwd.into(),
            ..Default::default()
        };
        let sessions = vec![
            sess("wide", "codex", "/srv"),
            sess("ignite", "claude", "/srv/px/"),
            sess("term", crate::agents::SHELL, "/srv/px/train"),
        ];
        assert_eq!(owner(p, &sessions).map(|s| s.name.as_str()), Some("ignite"));
        assert!(owner(q, &sessions).is_none());
    }

    #[test]
    fn pmon_and_clock_lines() {
        let mut st = Stats::default();
        let mut acc = Acc::default();
        let gpu = "G\t0, NVIDIA GeForce RTX 4090, 50, 1000, 24564, 44, 26.41, 450.00, 0, 00000000:01:00.0";
        let text = format!(
            "U\t1000\nC\t24\tIntel\t100\t4096\n\
Q\t4000\t5000+0\t100\tpython\tR\t8\t1\t50000\n{gpu}\ng\n\
A\t00000000:01:00.0\t4000\t2048\t\t/usr/bin/python3\na\n\
P\t# gpu         pid   type     sm    mem    enc    dec    jpg    ofa     fb   ccpm    command \n\
P\t# Idx           #    C/G      %      %      %      %      %      %     MB     MB    name \n\
P\t    0       4000     C     80     30      -      -      -      -   2048      0    python\n\
P\t    0      57638     G      -      -      -      -      -      -    303      0    chrome --type=g\n\
p\nF\t3412\t4890\t5200000\nE\t1\n"
        );
        feed(&mut acc, &mut st, &text, Instant::now());
        assert_eq!(st.gpu_procs.len(), 2, "{:?}", st.gpu_procs);
        let py = &st.gpu_procs[0];
        assert_eq!(
            (py.pid.as_str(), py.gpu.as_str(), py.kind.as_str()),
            ("4000", "0", "C")
        );
        assert_eq!((py.sm, py.mem_util, py.enc), (Some(0.8), Some(0.3), None));
        assert_eq!(py.sm_avg, Some(0.8));
        // 50 s of CPU over the 500 s since it started (tick 50000 of a 1000 s uptime)
        assert_eq!(py.age, Some(500));
        assert!(close(py.avg_cpu.unwrap(), 0.1));
        let gfx = &st.gpu_procs[1];
        assert_eq!(
            (gfx.kind.as_str(), gfx.name.as_str()),
            ("G", "chrome --type=g")
        );
        assert!(close(gfx.mem, 303.0 * 1048576.0) && gfx.sm.is_none());
        assert_eq!(
            (st.cpu_mhz, st.cpu_mhz_peak, st.cpu_mhz_max),
            (Some(3412.0), Some(4890.0), Some(5200.0))
        );
        assert_eq!(st.gpus[0].temp_hist, vec![44.0]);
        // the average keeps counting: an idle interval (`-`) is 0
        let again = "P\t# gpu pid type sm mem enc dec fb command\nP\t0 4000 C - - - - 2048 python\np\nE\t2\n";
        feed(&mut acc, &mut st, again, Instant::now());
        assert_eq!(st.gpu_procs[0].sm_avg, Some(0.4));
    }

    #[test]
    fn nvidia_extended_line() {
        let g = parse_nvidia(
            "1, NVIDIA H100 80GB HBM3, 97, 70000, 81559, 66, 690.12, 700.00, [N/A], 00000000:2A:00.0, 43, 1980, 2619, 1980, P0, 5, 16, 0, 3, 71, 0x0000000000000025, 0",
        )
        .unwrap();
        assert_eq!((g.index.as_str(), g.name.as_str()), ("1", "H100 80GB HBM3"));
        assert!(close(g.util, 0.97) && close(g.mem_total, 81559.0 * 1048576.0));
        assert_eq!(
            (g.temp, g.power, g.power_limit, g.fan),
            (Some(66.0), Some(690.12), Some(700.0), None)
        );
        assert_eq!(g.bus, "00000000:2A:00.0");
        assert_eq!(g.mem_util, Some(0.43));
        assert_eq!(
            (g.clock_sm, g.clock_mem, g.clock_sm_max),
            (Some(1980.0), Some(2619.0), Some(1980.0))
        );
        assert_eq!((g.pstate.as_str(), g.pcie.as_str()), ("P0", "gen5 x16"));
        assert_eq!((g.enc, g.dec), (Some(0.0), Some(0.03)));
        assert_eq!(g.temp_mem, Some(71.0));
        assert_eq!(g.throttle, vec!["power cap", "thermal (SW)"]); // 0x1 idle dropped
        assert_eq!(g.ecc, Some(0));
        // a GeForce: N/A spellings, idle-only throttle
        let g = parse_nvidia(
            "0, NVIDIA GeForce RTX 4090, 0, 1006, 24564, 44, 24.78, 450.00, 0, 00000000:01:00.0, 0, 210, 405, 3105, P8, 1, 16, 0, 0, N/A, 0x0000000000000001, [N/A]",
        )
        .unwrap();
        assert_eq!(g.temp_mem, None);
        assert!(g.throttle.is_empty());
        assert_eq!((g.ecc, g.pcie.as_str()), (None, "gen1 x16"));
        let g = parse_nvidia(
            "0, Tesla T4, [N/A], [N/A], 15360, [N/A], [Not Supported], [Not Supported], N/A, 00000000:00:04.0, [N/A], [N/A], [N/A], [N/A], [N/A], [N/A], [N/A], [N/A], [N/A], [N/A], [N/A], [N/A]",
        )
        .unwrap();
        assert_eq!(
            (g.util, g.temp, g.power, g.mem_util, g.clock_sm),
            (0.0, None, None, None, None)
        );
        assert!(g.pstate.is_empty() && g.pcie.is_empty() && g.throttle.is_empty());
        assert_eq!(
            throttle_words("0x00000000000001E8"),
            vec![
                "HW slowdown",
                "thermal (SW)",
                "thermal (HW)",
                "power brake",
                "display clock"
            ]
        );
        assert!(throttle_words("Not Active").is_empty());
    }

    #[test]
    fn nvidia_basic_fallback_line() {
        let st = parse(
            "H\tgpu\tLinux 5.4\tUbuntu 18.04\nG\t0, Tesla V100-SXM2-16GB, 35, 1024, 16160, 40, 56.10, 300.00, [N/A], 00000000:00:1E.0\n\
G\t1, Tesla V100-SXM2-16GB, 0, 0, 16160, 38, 40.00, 300.00, [N/A], 00000000:00:1F.0\ng\nE\t1\n",
        );
        assert_eq!(st.gpus.len(), 2);
        let g = &st.gpus[0];
        assert_eq!(
            (g.name.as_str(), g.vendor.as_str(), g.fan),
            ("Tesla V100-SXM2-16GB", "nvidia", None)
        );
        assert!(close(g.util, 0.35) && close(g.mem_used, 1024.0 * 1048576.0));
        assert_eq!((g.mem_util, g.clock_sm, g.ecc), (None, None, None));
        assert!(g.pstate.is_empty() && g.throttle.is_empty());
        assert_eq!(g.mem_hist.len(), 1);
        assert_eq!(g.power_hist, vec![56.1 / 300.0]);
        assert!(st.has_gpu_tool && st.gpu_note.is_none());
    }

    /// A Mac's GPU from the I/O Registry: `ioreg`'s output through the
    /// sampler's own awk (each accelerator a `Z` record), then the parser.
    #[test]
    fn an_apple_gpu_from_the_io_registry() {
        let f = SCRIPT
            .split("gpu_apple() {")
            .nth(1)
            .and_then(|r| r.split("\ngpu() {").next())
            .expect("gpu_apple in the script");
        let dir = std::env::temp_dir().join(format!("aleph-ioreg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // an Apple M4, then an Intel Mac's two (a Radeon with its model in
        // <"…">, an Intel one without a model or memory)
        std::fs::write(
            dir.join("ioreg.txt"),
            "+-o AGXAcceleratorG16G  <class AGXAcceleratorG16G, id 0x10000033c, registered>\n    {\n      \"model\" = \"Apple M4\"\n      \"gpu-core-count\" = 10\n      \"PerformanceStatistics\" = {\"In use system memory (driver)\"=0,\"Alloc system memory\"=2715156480,\"Tiler Utilization %\"=71,\"Renderer Utilization %\"=72,\"Device Utilization %\"=73,\"In use system memory\"=972898304}\n    }\n\
+-o AMDRadeonX6000_AMDNavi14GraphicsAccelerator  <class AMDRadeonX6000>\n    {\n      \"model\" = <\"AMD Radeon Pro 5500M\">\n      \"PerformanceStatistics\" = {\"Device Utilization %\"=12,\"In use system memory\"=1048576}\n    }\n\
+-o IntelAccelerator  <class IntelAccelerator>\n    {\n      \"PerformanceStatistics\" = {\"Device Utilization %\"=3}\n    }\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("ioreg"),
            format!("#!/bin/sh\ncat '{}'\n", dir.join("ioreg.txt").display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join("ioreg"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "TP=; MSZ=17179869184; gpu_apple() {{{f}\ngpu_apple"
            ))
            .env("PATH", format!("{}:/usr/bin:/bin", dir.display()))
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let z: Vec<&str> = text.lines().filter(|l| l.starts_with('Z')).collect();
        assert_eq!(z.len(), 3, "{text}");
        assert!(text.ends_with("g\n"), "{text}");
        let st = parse(&format!("H\tmac\tDarwin 25.0\tmacOS 26\n{text}E\t1\n"));
        assert_eq!(st.gpus.len(), 3);
        let g = &st.gpus[0];
        assert_eq!(
            (g.name.as_str(), g.vendor.as_str()),
            ("M4 · 10 cores", "apple")
        );
        assert!(
            close(g.util, 0.73)
                && close(g.renderer.unwrap(), 0.72)
                && close(g.tiler.unwrap(), 0.71)
        );
        assert!(close(g.mem_used, 972898304.0) && close(g.mem_total, 17179869184.0));
        assert_eq!((g.cores, g.mem_alloc), (Some(10), Some(2715156480.0)));
        assert_eq!(g.hist, vec![0.73]);
        assert_eq!(
            (st.gpus[1].name.as_str(), st.gpus[1].vendor.as_str()),
            ("AMD Radeon Pro 5500M", "amd")
        );
        assert_eq!((st.gpus[2].name.as_str(), st.gpus[2].util), ("GPU", 0.03));
        assert!(st.has_gpu_tool && st.gpu_note.is_none());
    }

    #[test]
    fn gpu_notes() {
        let st = parse(
            "H\tbox\tLinux 6.8\tUbuntu\nG\t0, NVIDIA A10, 5, 1, 2, 3, 4, 5, 6, 00000000:01:00.0\ng\nE\t1\n\
X\tnvidia\tFailed to initialize NVML: Driver/library version mismatch\ng\nE\t2\n",
        );
        assert!(st.gpus.is_empty(), "a failed query clears the GPUs");
        assert!(st.has_gpu_tool);
        assert_eq!(
            st.gpu_note.as_deref(),
            Some("Failed to initialize NVML: Driver/library version mismatch")
        );
        let st = parse("H\tbox\tLinux 6.8\tUbuntu\nX\tnone\t\nE\t1\n");
        assert_eq!(
            st.gpu_note.as_deref(),
            Some("no nvidia-smi or rocm-smi on this machine")
        );
        assert!(!st.has_gpu_tool);
        // a Mac whose GPU didn't answer ioreg (an Apple one does: Z lines)
        let st = parse("H\tmac\tDarwin 25.0.0\tmacOS\nX\tnone\t\nE\t1\n");
        assert!(st.gpu_note.unwrap().contains("ioreg"));
        let st = parse(
            "H\tbox\tLinux 6.8\tUbuntu\nX\tamd\tERROR: GPU[0] : Unable to open device\ng\nE\t1\n",
        );
        assert_eq!(
            st.gpu_note.as_deref(),
            Some("ERROR: GPU[0] : Unable to open device")
        );
    }

    #[test]
    fn rocm_csv() {
        // ROCm 5.x, the basic flag set
        let v5 = "H\tmi\tLinux 5.15\tUbuntu 22.04\n\
R\tdevice,Temperature (Sensor edge) (C),Temperature (Sensor junction) (C),Temperature (Sensor memory) (C),Average Graphics Package Power (W),GPU use (%),GPU memory use (%),VRAM Total Memory (B),VRAM Total Used Memory (B)\n\
R\tcard0,45.0,47.0,50.0,35.0,12,3,68702699520,10960896\n\
R\tcard1,N/A,40.0,N/A,N/A,0,0,68702699520,0\ng\nE\t1\n";
        let st = parse(v5);
        assert_eq!(st.gpus.len(), 2);
        let g = &st.gpus[0];
        assert_eq!(
            (g.index.as_str(), g.vendor.as_str(), g.name.as_str()),
            ("0", "amd", "AMD GPU")
        );
        assert_eq!(
            (g.temp, g.temp_mem, g.power),
            (Some(45.0), Some(50.0), Some(35.0))
        );
        assert!(
            close(g.util, 0.12)
                && close(g.mem_total, 68702699520.0)
                && close(g.mem_used, 10960896.0)
        );
        assert_eq!(g.mem_util, Some(0.03));
        assert_eq!(st.gpus[1].temp, Some(40.0), "edge N/A: junction");
        assert_eq!(st.gpus[1].power, None);
        // ROCm 6.x, the extended flag set: product names, max power, bus, fan, clocks
        let v6 = "H\tmi\tLinux 6.8\tUbuntu 24.04\n\
R\tdevice,Device Name,Device ID,Card Series,Card Model,Card Vendor,Card SKU,PCI Bus,Temperature (Sensor edge) (C),Temperature (Sensor junction) (C),Temperature (Sensor memory) (C),Fan speed (level),Fan speed (%),Fan RPM,fclk clock speed:,mclk clock speed:,sclk clock speed:,Max Graphics Package Power (W),Current Socket Graphics Package Power (W),GPU use (%),GPU Memory Allocated (VRAM%),GPU Memory Read/Write Activity (%),Memory Activity,VRAM Total Memory (B),VRAM Total Used Memory (B)\n\
R\tcard0,\"Navi 31 [Radeon RX 7900 XT/7900 XTX]\",0x744c,Navi 31 [Radeon RX 7900 XT/7900 XTX],0x5304,Advanced Micro Devices Inc. [AMD/ATI],D7070100,0000:03:00.0,52.0,61.0,70.0,64,25.1,1100,(1940Mhz),(1249Mhz),(2482Mhz),327.0,275.0,99,40,61,N/A,25753026560,10137600000\ng\nE\t1\n";
        let g = parse(v6).gpus.remove(0);
        assert_eq!(g.name, "Navi 31 [Radeon RX 7900 XT/7900 XTX]");
        assert_eq!(g.bus, "0000:03:00.0");
        assert_eq!(
            (g.power, g.power_limit, g.fan),
            (Some(275.0), Some(327.0), Some(25.1))
        );
        assert_eq!((g.clock_sm, g.clock_mem), (Some(2482.0), Some(1249.0)));
        assert_eq!((g.temp, g.temp_mem), (Some(52.0), Some(70.0)));
        assert!(close(g.util, 0.99));
        assert_eq!(g.mem_util, Some(0.61));
        assert_eq!(g.power_hist, vec![275.0 / 327.0]);
        // a section-per-flag layout merges by card
        let split = "R\tdevice,GPU use (%)\nR\tcard0,7\nR\tdevice,VRAM Total Memory (B),VRAM Total Used Memory (B)\nR\tcard0,100,25\ng\nE\t1\n";
        let g = parse(split).gpus.remove(0);
        assert!(close(g.util, 0.07) && close(g.mem_used / g.mem_total, 0.25));
    }

    /// A real capture from b31 (Ubuntu 24.04, i9-12900K, RTX 4090), trimmed to a
    /// dozen processes.
    #[test]
    fn real_linux_capture() {
        let st = parse(B31);
        assert_eq!(
            (st.platform.as_str(), st.ncpu, st.cores.len()),
            ("linux", 24, 24)
        );
        assert_eq!(st.cpu_model, "12th Gen Intel Core i9-12900K");
        assert_eq!(st.samples, 3);
        // Δ user 60, system 18, idle 2452
        assert!(close(st.cpu_user, 60.0 / 2530.0) && close(st.cpu_system, 18.0 / 2530.0));
        assert!(close(st.cpu, 78.0 / 2530.0));
        assert_eq!(st.cpu_temp, Some(29.0)); // the last sample with temps
        let labels: Vec<&str> = st.temps.iter().map(|t| t.0.as_str()).collect();
        assert_eq!(
            labels,
            ["cpu", "cores max", "acpi", "eno1", "nvme0", "nvme1", "wifi"]
        );
        assert!(st.mem_buffers > 0.0 && st.mem_cached > st.mem_buffers && st.mem_shared > 0.0);
        assert!(st.mem_available > 0.0 && st.mem_used > 0.0 && st.swap_used > 0.0);
        let nets: Vec<&str> = st.nets.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(nets, ["wlp0s20f3", "tailscale0"]);
        assert!(close(st.nets[0].rx, 1254.0) && close(st.nets[0].tx, 31052.0));
        let disks: Vec<&str> = st.disk_io.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(disks, ["nvme0n1", "nvme1n1"]);
        assert!(close(st.disk_io[0].write, 1776.0 * 512.0));
        assert_eq!(st.disk_io[0].busy, Some(0.002));
        assert_eq!(st.procs_total, 13);
        // the last sample: between the last two E lines
        let last = B31.trim_end().rsplit("\nE\t").nth(1).unwrap();
        let thr: usize = last
            .lines()
            .filter(|l| l.starts_with("Q\t"))
            .map(|l| l.split('\t').nth(6).unwrap().parse::<usize>().unwrap())
            .sum();
        assert_eq!(st.threads_total, thr);
        let uvm = st.procs.iter().find(|p| p.pid == "1641");
        assert!(
            uvm.is_none(),
            "kernel thread with no CPU and no RSS stays out"
        );
        let c = st.procs.iter().find(|p| p.pid == "3540145").unwrap();
        assert_eq!(
            (
                c.name.as_str(),
                c.state.as_str(),
                c.threads,
                c.user.as_str()
            ),
            ("claude", "S", 26, "selene")
        );
        assert!(c.cmd.starts_with("claude --resume ffcae966"));
        assert!(close(c.cpu, 0.03) && close(c.time, 283.94));
        assert_eq!(st.agent_procs.len(), 1);
        assert_eq!(
            (st.agent_procs[0].0.as_str(), st.agent_procs[0].1),
            ("claude", 2)
        );
        assert_eq!(st.claude_procs, 2);
        let g = &st.gpus[0];
        assert_eq!(
            (g.name.as_str(), g.pstate.as_str(), g.pcie.as_str()),
            ("RTX 4090", "P8", "gen1 x16")
        );
        assert_eq!(
            (g.clock_sm, g.clock_mem, g.clock_sm_max),
            (Some(210.0), Some(405.0), Some(3105.0))
        );
        assert_eq!(
            (g.temp_mem, g.ecc, g.mem_util, g.enc),
            (None, None, Some(0.0), Some(0.0))
        );
        assert!(g.throttle.is_empty());
        assert_eq!(
            (g.hist.len(), g.mem_hist.len(), g.power_hist.len()),
            (3, 3, 3)
        );
        assert_eq!(st.gpu_procs.len(), 1);
        assert_eq!(
            (st.gpu_procs[0].gpu.as_str(), st.gpu_procs[0].user.as_str()),
            ("0", "selene")
        );
        assert_eq!(st.users_logged, Some(2));
        assert_eq!(
            st.sessions_today,
            vec![("claude".into(), 7), ("codex".into(), 0)]
        );
        assert_eq!(st.convos_today, Some(7));
        assert_eq!(st.disks.len(), 1);
        assert_eq!(st.gpu_note, None);
    }

    /// A real capture from a Mac (Apple M4), trimmed to a few processes.
    #[test]
    fn real_macos_capture() {
        let st = parse(MACOS);
        assert_eq!(
            (st.platform.as_str(), st.os.as_str(), st.cpu_model.as_str()),
            ("macos", "macOS 27.0", "Apple M4")
        );
        // iostat's split over the last pause
        assert!(close(st.cpu, 0.2) && close(st.cpu_user, 0.15) && close(st.cpu_system, 0.06));
        assert!(st.cores.is_empty() && st.cpu_temp.is_none());
        assert!(
            close(st.mem_total, 16777216.0 * 1024.0) && close(st.mem_available, 4701696.0 * 1024.0)
        );
        assert!(close(st.mem_cached, 3405552.0 * 1024.0));
        assert!(close(st.swap_used, (3145728.0 - 1630525.0) * 1024.0));
        assert_eq!(st.load, [3.45, 3.70, 4.09]);
        let nets: Vec<&str> = st.nets.iter().map(|n| n.name.as_str()).collect();
        assert!(nets.contains(&"en0") && nets.contains(&"utun2"), "{nets:?}");
        assert!(!nets
            .iter()
            .any(|n| n.starts_with("lo") || n.starts_with("awdl") || n.starts_with("anpi")));
        assert!(
            !nets.contains(&"utun0") && !nets.contains(&"nan0"),
            "one-way links stay out"
        );
        let en0 = st.nets.iter().find(|n| n.name == "en0").unwrap();
        assert!(close(en0.rx, 301404.0) && close(en0.tx, 44301.0));
        assert_eq!(st.disk_io.len(), 1);
        let d = &st.disk_io[0];
        assert!(close(d.read, 64.0 * 512.0) && close(d.write, 2232.0 * 512.0) && d.busy.is_none());
        assert_eq!((st.procs_total, st.procs_running), (5, 1));
        let p = |pid: &str| st.procs.iter().find(|p| p.pid == pid).unwrap();
        let r = p("18506");
        assert_eq!(r.name, "Browser Helper (Renderer)");
        assert_eq!((r.threads, r.state.as_str()), (20, "S"));
        let c = p("16298");
        assert!(
            close(c.cpu, 0.33) && close(c.time, 556.58),
            "{} {}",
            c.cpu,
            c.time
        );
        assert_eq!(
            (c.threads, c.user.as_str(), c.cmd.as_str()),
            (23, "alexandersmith", "claude")
        );
        assert_eq!(st.top[0].pid, "16298");
        assert_eq!(st.threads_total, 4 + 22 + 20 + 19 + 23);
        assert_eq!(
            st.agent_procs,
            vec![("claude".into(), 1, 707744.0 * 1024.0)]
        );
        assert_eq!(st.claude_procs, 1);
        // (recorded before Apple GPUs were read: its sampler said none)
        assert!(st.gpu_note.unwrap().contains("ioreg"));
        assert_eq!(
            st.sessions_today,
            vec![("claude".into(), 2), ("codex".into(), 9)]
        );
        assert_eq!(st.users_logged, Some(4));
        assert_eq!(st.disks.len(), 2);
    }

    #[test]
    fn agent_matching() {
        let m = agent_of;
        // native executables
        assert_eq!(m("claude", "claude --resume"), Some("claude"));
        assert_eq!(m("codex", ""), Some("codex"));
        assert_eq!(m("cursor-agent", ""), Some("cursor"));
        assert_eq!(m("kiro-cli", ""), Some("kiro"));
        assert_eq!(m("crush", ""), Some("crush"));
        assert_eq!(m("muse", ""), Some("muse"));
        assert_eq!(m("prime-agent", ""), Some("prime"));
        assert_eq!(m("command-code", ""), Some("commandcode"));
        assert_eq!(m("/opt/homebrew/bin/opencode", ""), Some("opencode"));
        // argv[0] when the name is something else (macOS full paths, renamed threads)
        assert_eq!(m("2.1.3", "/Users/me/.local/bin/claude -c"), Some("claude"));
        // node / bun / python running an agent's script
        assert_eq!(
            m("node", "node /usr/local/bin/gemini --yolo"),
            Some("gemini")
        );
        assert_eq!(
            m(
                "node",
                "/usr/bin/node --no-warnings /home/u/.npm-global/bin/copilot"
            ),
            Some("copilot")
        );
        assert_eq!(
            m(
                "node",
                "node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js"
            ),
            Some("claude")
        );
        assert_eq!(
            m(
                "node",
                "node /usr/lib/node_modules/@openai/codex/bin/codex.js"
            ),
            Some("codex")
        );
        assert_eq!(
            m(
                "node",
                "node /x/node_modules/@qwen-code/qwen-code/dist/index.js"
            ),
            Some("qwen")
        );
        assert_eq!(
            m("node", "node /x/node_modules/@kilocode/cli/index.js"),
            Some("kilo")
        );
        assert_eq!(
            m("node", "node /x/node_modules/opencode-ai/bin/index.mjs"),
            Some("opencode")
        );
        assert_eq!(
            m(
                "node",
                "node /x/node_modules/@mariozechner/pi-coding-agent/dist/cli.js"
            ),
            Some("pi")
        );
        assert_eq!(
            m(
                "bun",
                "bun /x/node_modules/@letta-ai/letta-code/dist/main.js"
            ),
            Some("letta")
        );
        assert_eq!(
            m(
                "python3",
                "/home/u/.local/share/uv/tools/kimi-cli/bin/python /home/u/.local/bin/kimi"
            ),
            Some("kimi")
        );
        // not agents
        assert_eq!(m("node", "node server.js"), None);
        assert_eq!(m("node", ""), None);
        assert_eq!(
            m("cc", "cc -O2 main.c"),
            None,
            "aliases don't match process names"
        );
        assert_eq!(
            m(
                "Codex (Service)",
                "/Applications/ChatGPT.app/Contents/MacOS/Codex (Service)"
            ),
            None
        );
        assert_eq!(m("aleph", "aleph ls"), None);
        assert_eq!(m("python3", "python3 /srv/cmd/main.py"), None);
        // counting: children of the same agent add memory only
        let t = agent_table(&[
            ("10".into(), "1".into(), 100.0, "codex"),
            ("11".into(), "10".into(), 50.0, "codex"),
            ("20".into(), "1".into(), 10.0, "claude"),
            ("21".into(), "1".into(), 10.0, "claude"),
            ("30".into(), "1".into(), 5.0, "gemini"),
        ]);
        assert_eq!(
            t,
            vec![
                ("claude".into(), 2, 20.0),
                ("codex".into(), 1, 150.0),
                ("gemini".into(), 1, 5.0)
            ]
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(cputime("50:40.19"), Some(3040.19));
        assert_eq!(cputime("01:02:03"), Some(3723.0));
        assert_eq!(cputime("2-00:00:01"), Some(172801.0));
        assert_eq!(cputime("x"), None);
        assert_eq!(first_num("(1700Mhz)"), Some(1700.0));
        assert_eq!(first_num("N/A"), None);
        assert_eq!(platform_of("FreeBSD 14.1"), "freebsd");
        assert!(
            skip_iface("docker0", 1, 1) && skip_iface("eth1", 0, 0) && skip_iface("utun3", 0, 9672)
        );
        assert!(!skip_iface("en0", 1, 1) && !skip_iface("tailscale0", 5, 5));
        let t = |dir: &str, chip: &str, label: &str, c: f64| RawTemp {
            dir: dir.into(),
            chip: chip.into(),
            sensor: "temp1".into(),
            label: label.into(),
            c,
        };
        // AMD: Tdie over Tctl (which carries an offset), CCDs listed
        let (cpu, v) = temps_from(&[
            t("hwmon1", "k10temp", "Tctl", 80.0),
            t("hwmon1", "k10temp", "Tdie", 70.0),
            RawTemp {
                sensor: "temp3".into(),
                ..t("hwmon1", "k10temp", "Tccd1", 65.0)
            },
        ]);
        assert_eq!(cpu, Some(70.0));
        assert_eq!(v, vec![("cpu".into(), 70.0), ("ccd1".into(), 65.0)]);
        // no hwmon at all: thermal zones (a Raspberry Pi, a VM)
        let (cpu, v) = temps_from(&[
            t("thermal_zone0", "zone", "cpu-thermal", 48.3),
            t("thermal_zone1", "zone", "", -1.0),
        ]);
        assert_eq!(cpu, Some(48.3));
        assert_eq!(v, vec![("cpu-thermal".into(), 48.3)]);
    }

    #[test]
    fn script_is_posix_sh() {
        let s = script(2);
        assert!(!s.contains('@'), "unfilled placeholder");
        for sh in ["sh", "dash", "bash"] {
            let Ok(mut c) = Command::new(sh)
                .arg("-n")
                .stdin(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            else {
                continue; // that shell isn't installed
            };
            c.stdin.take().unwrap().write_all(s.as_bytes()).unwrap();
            let o = c.wait_with_output().unwrap();
            assert!(
                o.status.success(),
                "{sh} -n: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        }
    }

    const TAGS: &[&str] = &[
        "H", "C", "S", "M", "L", "U", "N", "B", "Y", "F", "Q", "T", "J", "W", "c", "G", "R", "g",
        "A", "a", "P", "p", "X", "Z", "D", "O", "K", "E",
    ];

    #[test]
    fn script_runs_locally() {
        let out = run_local(2, None);
        let tags: std::collections::HashSet<&str> =
            out.lines().map(|l| l.split('\t').next().unwrap()).collect();
        for t in &tags {
            assert!(TAGS.contains(t), "unknown line in the stream: {t:?}");
        }
        for t in ["H", "C", "M", "L", "U", "N", "W", "D", "E"] {
            assert!(tags.contains(t), "no {t} line:\n{out}");
        }
        if cfg!(target_os = "linux") {
            for t in ["S", "Q"] {
                assert!(tags.contains(t), "no {t} line");
            }
        }
        if cfg!(target_os = "macos") {
            for t in ["T", "J", "c", "B"] {
                assert!(tags.contains(t), "no {t} line");
            }
            // its GPU from the I/O Registry (or why not)
            assert!(tags.contains("Z") || tags.contains("X"), "no Z or X line");
        }
        let mut st = Stats::default();
        let mut acc = Acc::default();
        for l in out.lines() {
            acc.line(&mut st, l);
        }
        assert_eq!(st.samples, 2, "sampler produced {} samples", st.samples);
        assert!(st.mem_total > 0.0 && st.ncpu > 0 && st.procs_total > 0);
        assert!(!st.procs.is_empty() && !st.top.is_empty());
        assert!(!st.platform.is_empty() && st.load_hist.len() == 2);
        // process ages (Linux start ticks, macOS etime) and lifetime CPU
        assert!(st
            .procs
            .iter()
            .any(|p| p.age.is_some_and(|a| a > 0) && p.avg_cpu.is_some()));
        if cfg!(target_os = "macos") {
            assert!(st.mem_available > 0.0 && st.threads_total > 0);
            assert!(st.gpu_note.is_some() || st.has_gpu_tool);
        }
    }

    /// No ps, getconf, grep, df, who, find, sysctl, GPU tools: still a clean stream.
    #[test]
    fn script_survives_a_bare_machine() {
        let dir = std::env::temp_dir().join(format!("ch-telemetry-bare-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for tool in ["awk", "cat", "sleep", "uname", "date", "tr"] {
            for d in ["/usr/bin", "/bin"] {
                let p = std::path::Path::new(d).join(tool);
                if p.exists() {
                    let _ = std::os::unix::fs::symlink(&p, dir.join(tool));
                    break;
                }
            }
        }
        let out = run_local(2, dir.to_str());
        let _ = std::fs::remove_dir_all(&dir);
        for l in out.lines() {
            let t = l.split('\t').next().unwrap();
            assert!(TAGS.contains(&t), "junk in the stream: {l:?}");
        }
        assert_eq!(
            out.lines().filter(|l| l.starts_with('E')).count(),
            2,
            "{out}"
        );
        assert!(parse(&out).samples == 2);
    }

    /// `TELEMETRY_SCRIPT=file cargo test -- --ignored write_script`: the sampler,
    /// bounded to 6 samples, for `ssh host sh -s < file > capture`.
    #[test]
    #[ignore]
    fn write_script() {
        if let Ok(p) = std::env::var("TELEMETRY_SCRIPT") {
            std::fs::write(p, bounded_script(6)).unwrap();
        }
    }

    /// `TELEMETRY_CAPTURE=file cargo test -- --ignored parse_capture --nocapture`:
    /// a summary of a raw capture (`ssh host sh -s < script > file`).
    #[test]
    #[ignore]
    fn parse_capture() {
        let Ok(p) = std::env::var("TELEMETRY_CAPTURE") else {
            return;
        };
        let st = parse(&std::fs::read_to_string(p).unwrap());
        println!(
            "{} ({}) {} · {} samples",
            st.hostname, st.platform, st.os, st.samples
        );
        println!(
            "cpu {:.1}% (user {:.1} sys {:.1} iowait {:.1} steal {:.1}) · {} cores · {} °C · load {:?}",
            st.cpu * 100.0,
            st.cpu_user * 100.0,
            st.cpu_system * 100.0,
            st.cpu_iowait * 100.0,
            st.cpu_steal * 100.0,
            st.cores.len(),
            st.cpu_temp.unwrap_or(f64::NAN),
            st.load
        );
        println!("temps {:?}", st.temps);
        println!(
            "mem used {:.1} / {:.1} GiB · avail {:.1} · buffers {:.1} · cached {:.1} · shared {:.1} · swap {:.1}/{:.1}",
            st.mem_used / 1073741824.0,
            st.mem_total / 1073741824.0,
            st.mem_available / 1073741824.0,
            st.mem_buffers / 1073741824.0,
            st.mem_cached / 1073741824.0,
            st.mem_shared / 1073741824.0,
            st.swap_used / 1073741824.0,
            st.swap_total / 1073741824.0
        );
        println!(
            "tasks {} · {} running · {} threads · {:?} users",
            st.procs_total, st.procs_running, st.threads_total, st.users_logged
        );
        for n in &st.nets {
            println!(
                "net {:<12} rx {:>10.0} B/s tx {:>10.0} B/s",
                n.name, n.rx, n.tx
            );
        }
        for d in &st.disk_io {
            println!(
                "disk {:<10} r {:>10.0} B/s w {:>10.0} B/s busy {:?}",
                d.name, d.read, d.write, d.busy
            );
        }
        for g in &st.gpus {
            println!(
                "gpu{} {} [{}] util {:.0}% mem {:.1}/{:.1} GiB {:?}°C {:?}/{:?} W fan {:?} memutil {:?} sm {:?}/{:?} mem {:?} MHz {} {} enc {:?} dec {:?} memtemp {:?} throttle {:?} ecc {:?}",
                g.index,
                g.name,
                g.vendor,
                g.util * 100.0,
                g.mem_used / 1073741824.0,
                g.mem_total / 1073741824.0,
                g.temp,
                g.power,
                g.power_limit,
                g.fan,
                g.mem_util,
                g.clock_sm,
                g.clock_sm_max,
                g.clock_mem,
                g.pstate,
                g.pcie,
                g.enc,
                g.dec,
                g.temp_mem,
                g.throttle,
                g.ecc
            );
        }
        for p in &st.gpu_procs {
            println!(
                "gpu proc gpu{} {} {} {:.0} MiB {}",
                p.gpu,
                p.pid,
                p.user,
                p.mem / 1048576.0,
                p.name
            );
        }
        for p in st.procs.iter().take(8) {
            println!(
                "proc {:>8} {:<10} {:<16} {} thr {:>3} cpu {:>5.1}% rss {:>6.0} MiB time {:>9.1}s  {}",
                p.pid,
                p.user,
                p.name,
                p.state,
                p.threads,
                p.cpu * 100.0,
                p.rss / 1048576.0,
                p.time,
                ui::trunc(&p.cmd, 50)
            );
        }
        println!(
            "agents {:?} · sessions {:?} · gpu note {:?}",
            st.agent_procs, st.sessions_today, st.gpu_note
        );
    }
    #[test]
    fn agents_inside_and_outside_herdr() {
        let mut parents: HashMap<String, (String, String)> = HashMap::new();
        for (pid, ppid, name) in [
            ("100", "1", "herdr"),
            ("110", "100", "zsh"),
            ("120", "110", "claude"),
            ("121", "120", "claude"),
            ("200", "1", "login"),
            ("210", "200", "-zsh"),
            ("220", "210", "codex"),
        ] {
            parents.insert(pid.into(), (ppid.into(), name.into()));
        }
        let info: HashMap<String, PInfo> = [("120", 3600u64), ("220", 60)]
            .into_iter()
            .map(|(p, age)| {
                (
                    p.to_string(),
                    PInfo {
                        cpu: 0.0,
                        avg_cpu: None,
                        age: Some(age),
                        user: "selene".into(),
                        cmd: "claude --resume x".into(),
                        name: String::new(),
                        ppid: String::new(),
                    },
                )
            })
            .collect();
        let procs = vec![
            ("120".to_string(), "110".to_string(), 1.0, "claude"),
            // its helper (a child that's the same agent): left out
            ("121".to_string(), "120".to_string(), 1.0, "claude"),
            ("220".to_string(), "210".to_string(), 1.0, "codex"),
        ];
        let v = agent_list(&procs, &parents, &info);
        assert_eq!(v.len(), 2);
        assert!(helper("/Users/u/.local/bin/claude --chrome-native-host"));
        assert!(helper("codex mcp serve"));
        assert!(!helper("claude --resume x"));
        // Claude Code's background daemon and terminal hosts aren't sessions
        assert!(helper(
            "/home/u/.local/bin/claude daemon run --json-path /home/u/.claude/daemon.json"
        ));
        assert!(helper(
            "claude bg-pty-host --bg-pty-host /tmp/cc-daemon-1000/8e/pty/954"
        ));
        assert!(helper(
            "claude bg-spare --bg-spare /tmp/cc-daemon-1000/8e6565ad/spare/1"
        ));
        assert!(!helper("claude --resume daemon-run-fix"));
        assert!(helper(
            "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex sandbox -c x=1 -- node kernel.js"
        ));
        assert!(!helper("codex exec fix the sandbox"));
        assert_eq!((v[0].agent.as_str(), v[0].in_herdr), ("claude", true));
        assert_eq!((v[1].agent.as_str(), v[1].in_herdr), ("codex", false));
        assert_eq!(v[1].age, Some(60));
    }

    #[test]
    fn claudes_background_daemon_is_no_session_but_one_it_hosts_is() {
        let mut parents: HashMap<String, (String, String)> = HashMap::new();
        for (pid, ppid, name) in [
            ("300", "1", "claude"),
            ("310", "300", "claude"),
            ("311", "310", "claude"),
            ("400", "1", "claude"),
        ] {
            parents.insert(pid.into(), (ppid.into(), name.into()));
        }
        let pi = |cmd: &str| PInfo {
            cpu: 0.0,
            avg_cpu: None,
            age: Some(60),
            user: "selene".into(),
            cmd: cmd.into(),
            name: String::new(),
            ppid: String::new(),
        };
        let info: HashMap<String, PInfo> = [
            (
                "300",
                "claude daemon run --json-path /home/u/.claude/d.json",
            ),
            (
                "310",
                "claude bg-pty-host --bg-pty-host /tmp/cc-daemon-1000/8e/pty/9",
            ),
            ("311", "claude --resume 0c1f"),
            (
                "400",
                "claude bg-pty-host --bg-pty-host /tmp/cc-daemon-1000/8e/pty/7",
            ),
        ]
        .into_iter()
        .map(|(p, c)| (p.to_string(), pi(c)))
        .collect();
        let procs: Vec<(String, String, f64, &'static str)> = ["300", "310", "311", "400"]
            .into_iter()
            .map(|p| (p.to_string(), parents[p].0.clone(), 1.0, "claude"))
            .collect();
        let v = agent_list(&procs, &parents, &info);
        // the daemon and its terminal hosts left out; the session it hosts kept
        let pids: Vec<&str> = v.iter().map(|a| a.pid.as_str()).collect();
        assert_eq!(pids, ["311"]);
    }
}

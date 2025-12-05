#!/var/lib/data/raku/bin/raku
#!/var/lib/data/raku/maxzef/bin/raku
# vmstat-free-score-ema.raku
# Final freedom score (0..100, higher = more idle) using hysteresis over N samples.

#use Data::Dump::Tree;
#use Grammar::Debugger;

my DateTime $now;
my DateTime $maintenance-window-open;
my Int      $maintenance-window-duration-seconds;

#   /usr/bin/rebootmgrctl get-window
#   Maintenance window is set to '*-*-* 01:00:00', lasting 00:15.
grammar REBOOTMGRWINDOW-grammar {
    token TOP   { ^ 'Maintenance window is set to ' '\'*-*-* ' <wtime> '\', lasting ' <wdur> '.' $ }
    token wtime { <whour> ':' <wmin> ':' <wsec> }
    token whour { \d\d }
    token wmin  { \d\d }
    token wsec  { \d\d }
    token wdur  { <dhour> ':' <dmin> }
    token dhour { \d\d }
    token dmin  { \d\d }
}

class REBOOTMGRWINDOW-actions {
    method wtime ($/) {
        $maintenance-window-open =
            DateTime.new(:year($now.year), :month($now.month), :day($now.day),
                         :hour($/<whour>),
                         :minute($/<wmin>),
                         :second($/<wsec>),
                         :timezone($*TZ),
                        );
    }
    method wdur ($/) {
        $maintenance-window-duration-seconds = (+$/<dhour> * 60 * 60) + (+$/<dmin> * 60);
    }
}

$now        = DateTime.new(now);
my $proc    = run '/usr/bin/rebootmgrctl', 'get-window', :out, :err;
my $out     = $proc.out.slurp(:close);
my $err     = $proc.err.slurp(:close);
if $proc.exitcode != 0 {
    note "/usr/bin/rebootmgrctl get-window error: {$err} {$proc.exitcode}";
}
else {
    if my $match        = REBOOTMGRWINDOW-grammar.parse($out.trim, :actions(REBOOTMGRWINDOW-actions)) {
        my $proximity   = $maintenance-window-open - $now;
        if $proximity < 0 {
            if $proximity.abs <= (15 * 60) {
                my $valkey = sprintf "valkey-cli -h 172.19.2.254 --raw ZADD RSE^worker-node-candidates %.2f %s", 0, $*KERNEL.hostname;
                qqx{$valkey};
                exit 0;
            }
    }
    else { die; }
}

# ----------------------------
# Sampling configuration
# ----------------------------
constant ITERATIONS        = 5;      # number of per-interval samples
constant SAMPLE_DELAY_SEC  = 1;      # delay between vmstat samples (seconds)

# Hysteresis configuration (EMA)
constant EMA_ALPHA         = 0.20;   # weight of the new sample in EMA (0<alpha<=1)
constant USE_STEP_CLAMP    = True;   # limit per-step change to avoid flapping
constant MAX_STEP_DELTA    = 10.0;   # max +/- change allowed per sample (points)

# ----------------------------
# Tunable normalization ceilings (absurdly high by design)
# ----------------------------
constant R_MAX              = 1000;      # runnable procs ceiling
constant B_MAX              = 1000;      # blocked tasks ceiling
constant SI_MAX_KBPS        = 16384;     # swap-in KB/s ceiling
constant SO_MAX_KBPS        = 16384;     # swap-out KB/s ceiling
constant SWPD_MAX_KB        = 1048576;   # swap used KB ceiling (~1 GiB)
constant IO_BLOCKS_MAX      = 100_000;   # bi+bo blocks/s ceiling
constant CS_HIGH_PER_CPU    = 100_000;   # context switches/s per CPU ceiling
constant IN_HIGH_PER_CPU    = 50_000;    # interrupts/s per CPU ceiling

# ----------------------------
# Category weights for final score (sum to 1)
# ----------------------------
constant W_PROCS  = 0.20;
constant W_MEMORY = 0.15;
constant W_SWAP   = 0.10;
constant W_IO     = 0.25;
constant W_SYSTEM = 0.10;
constant W_CPU    = 0.20;

# ----------------------------
# Helpers
# ----------------------------
sub clamp(Real $x, :$min = 0, :$max = 100) {
    my ($lo, $hi) = $min <= $max ?? ($min, $max) !! ($max, $min);
    (($x max $lo) min $hi).Num
}
sub score-inverse(Real $v, Real $max)             { 100 - ((($v min $max) / $max) * 100) }  # 0 -> 100; big -> 0
sub score-percent-inverse(Real $p)                { 100 - clamp($p) }                        # % busy -> % free
sub avg(@vals)                                    { @vals.elems ?? @vals.sum / @vals.elems !! 0 }

# More robust CPU thread count:
# 1) /sys/devices/system/cpu/online (e.g., "0-7", "0,2-3")
# 2) fallback: count cpuN lines in /proc/stat
sub cpu-count() {
    my $online = '/sys/devices/system/cpu/online';
    if $online.IO.f {
        my $s = $online.IO.slurp.trim;
        my @ranges = $s.split(',');
        my $count = 0;
        for @ranges -> $r {
            if $r ~~ / ^ (\d+) '-' (\d+) $ / {
                my ($lo, $hi) = $0.Int, $1.Int;
                $count += ($hi - $lo + 1) if $hi >= $lo;
            } elsif $r ~~ /^(\d+)$/ {
                $count += 1;
            }
        }
        return $count if $count > 0;
    }
    my $stat = '/proc/stat';
    return 1 unless $stat.IO.f;
    return lines($stat).grep(*.match(/^cpu\d+/)).elems || 1;
}

# Compute one freedom score from a single vmstat data line + header columns
sub compute-score(@cols, Str $data-line, Int $CPUs) returns Real {
    my @vals = $data-line.words;
    die "Column count mismatch: header{@cols.elems} vs data{@vals.elems}" if @cols.elems != @vals.elems;
    my %m = @cols Z=> @vals ==> map { .key => (+.value) };

    # Defaults for optional fields
    for <st gu in cs us sy id wa bi bo si so swpd r b free buff cache> -> $k {
        %m{$k} //= 0;
    }

    # Procs
    my $procs-score = avg([
        score-inverse(+%m<r>, R_MAX),
        score-inverse(+%m<b>, B_MAX),
    ]);

    # Memory
    my $memory-score = avg([
        score-inverse(+%m<si>, SI_MAX_KBPS),
        score-inverse(+%m<so>, SO_MAX_KBPS),
    ]);

    # Swap
    my $swap-traffic-free = score-inverse(%m<si> + %m<so>, SI_MAX_KBPS + SO_MAX_KBPS);
    my $swap-used-free    = score-inverse(+%m<swpd>, SWPD_MAX_KB);
    my $swap-score        = avg([$swap-traffic-free, $swap-used-free]);

    # IO
    my $io-free-wa        = score-percent-inverse(+%m<wa>);
    my $io-free-blocked   = score-inverse(+%m<b>, B_MAX);
    my $io-free-activity  = score-inverse(%m<bi> + %m<bo>, IO_BLOCKS_MAX);
    my $io-score          = avg([$io-free-wa, $io-free-blocked, $io-free-activity]);

    # System (per CPU)
    my $cs-per-cpu        = %m<cs> / $CPUs;
    my $in-per-cpu        = %m<in> / $CPUs;
    my $sys-free-cs       = score-inverse($cs-per-cpu, CS_HIGH_PER_CPU);
    my $sys-free-in       = score-inverse($in-per-cpu, IN_HIGH_PER_CPU);
    my $system-score      = avg([$sys-free-cs, $sys-free-in]);

    # CPU: id is already "% free"; subtract tiny penalty for st+gu if present
    my $cpu-id-free       = clamp(+%m<id>);
    my $cpu-penalty       = (%m<st> + %m<gu>) min 10;
    my $cpu-score         = clamp($cpu-id-free - $cpu-penalty);

    # Final weighted freedom score
    return (
        W_PROCS  * $procs-score +
        W_MEMORY * $memory-score +
        W_SWAP   * $swap-score +
        W_IO     * $io-score +
        W_SYSTEM * $system-score +
        W_CPU    * $cpu-score
    );
}

# ----------------------------
# Run vmstat -y with delay & count, parse lines
# ----------------------------
my $CPUs = cpu-count();

# We expect: decorative header line, column header ("r b swpd ..."), then N data lines
my $cmd = 'vmstat -y ' ~ SAMPLE_DELAY_SEC ~ ' ' ~ ITERATIONS;
my @lines = qqx{$cmd}.lines.grep(*.trim);
die "vmstat produced no output" unless @lines.elems >= 3;

# Find the column header line (starts with 'r')
my Int $hdr-idx = -1;
for ^@lines.elems -> $i {
    my @w = @lines[$i].words;
    if @w && @w[0] eq 'r' {
        $hdr-idx = $i;
        last;
    }
}
die "Could not locate vmstat column header line" if $hdr-idx < 0;

my @cols = @lines[$hdr-idx].words;
my @data-lines = @lines[ ($hdr-idx + 1) .. * ];

# If more lines than ITERATIONS arrived (e.g., terminal quirks), trim
@data-lines = @data-lines[ ^ITERATIONS ] if @data-lines.elems > ITERATIONS;
die "Insufficient data lines: expected {ITERATIONS}, got {@data-lines.elems}" if @data-lines.elems < ITERATIONS;

# ----------------------------
# Hysteresis: EMA over the per-interval scores
# ----------------------------
my Real $ema = compute-score(@cols, @data-lines[0], $CPUs);
for @data-lines[1..*] -> $dl {
    my Real $s = compute-score(@cols, $dl, $CPUs);
    my Real $delta = $s - $ema;
    if USE_STEP_CLAMP {
        $delta = clamp($delta, :min(-MAX_STEP_DELTA), :max(MAX_STEP_DELTA));
    }
    $ema += EMA_ALPHA * $delta;  # EMA update: new-score influence weighted by alpha
}

# Final cumulative freedom score (print only the numeric value)
my $valkey = sprintf "valkey-cli -h 172.19.2.254 --raw ZADD RSE^worker-node-candidates %.2f %s", clamp($ema), $*KERNEL.hostname;

qqx{$valkey}

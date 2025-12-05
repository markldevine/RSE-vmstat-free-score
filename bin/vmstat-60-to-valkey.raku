#!/var/lib/data/raku/maxzef/bin/raku
#!/var/lib/data/raku/bin/raku

use Redis;
use MicroOS::vmstat::record;

my $valkey-list = 'RSE^statistics^' ~ $*KERNEL.hostname ~ '^vmstat^rollingsixty';
my $valkey      = Redis.new("172.19.2.254:6379", :decode_response);
$valkey.del($valkey-list) if $valkey.exists($valkey-list);
$valkey.rpush($valkey-list, (0 xx 60));

#   ~> vmstat -y -n -t 1 2
#   procs -----------memory---------- ---swap-- -----io---- -system-- -------cpu------- -----timestamp-----
#    r  b   swpd   free   buff  cache   si   so    bi    bo   in   cs us sy id wa st gu                 EST
#    0  0      0 236288   2800 220600    0    0     0     0  563  335  0  0 99  0  0  0 2025-12-03 08:38:29
#    0  0      0 236952   2800 220600    0    0     0     0  532  319  0  0 100 0  0  0 2025-12-03 08:38:30

my $proc = Proc::Async.new('/usr/bin/vmstat', '-y', '-n', '-t', 1);

react {
    whenever $proc.stdout -> $line {
        if $line ~~ /^ \s* \d+ \s+ \d+ \s+ \d+/ {
            my $v-record = MicroOS::vmstat::record.new(|(<v-r v-b v-swpd v-free v-buff v-cache v-si v-so v-bi v-bo v-in v-cs v-us v-sy v-id v-wa v-st v-gu v-date v-time> Z=> $line.words).Hash);
            $valkey.lset($valkey-list, +$v-record.v-datetime.second, $v-record.marshal) or note 'valkey LSET ' ~ $v-record.v-datetime.second ~ ' failed';
        }
    }

    whenever $proc.stderr -> $err {
        note "Error: {$err.trim}";
        done;
    }

    whenever $proc.start -> $promise {
        whenever $promise -> $status {
            if $status.signal {
                note 'vmstat was killed by signal: ' ~ $status.signal;
            }
            elsif $status.exit {
                note 'vmstat exited normally with code: ' ~ $status.exitcode;
            }
            else {
                note 'vmstat ended unexpectedly';
            }
            done;
        }
    }
}

$valkey.quit;
exit 1;                                                                                         # should never exit...

=finish


unit class MicroOS::vmstat::record;

# --- Attribute types (final, enforced by BUILD/TWEAK) ---
has Int $.v-r;     # processes waiting for run time
has Int $.v-b;     # processes in uninterruptible sleep
has Int $.v-swpd;  # amount of virtual memory used
has Int $.v-free;  # amount of idle memory
has Int $.v-buff;  # memory used as buffers
has Int $.v-cache; # memory used as cache
has Int $.v-si;    # swap in from disk
has Int $.v-so;    # swap out to disk
has Int $.v-bi;    # blocks received from a block device
has Int $.v-bo;    # blocks sent to a block device
has Int $.v-in;    # interrupts per second
has Int $.v-cs;    # context switches per second
has Int $.v-us;    # user time (%)
has Int $.v-sy;    # system time (%)
has Int $.v-id;    # idle time (%)
has Int $.v-wa;    # IO wait time (%)
has Int $.v-st;    # stolen time (%)
has Int $.v-gu;    # guest time (%) — if present in your flavor
has Str $.v-date;  # keep raw, or change to Date/DateTime if you prefer
has Str $.v-time;

# --- Optional: domain constraints using subsets (customize as you like) ---
subset Percent where 0 .. 100;

# If you want percentage attributes constrained strictly:
# has Percent $.v-us;
# has Percent $.v-sy;
# has Percent $.v-id;
# has Percent $.v-wa;
# has Percent $.v-st;
# has Percent $.v-gu;

# --- BUILD: accept strings/anything; parse, coerce, validate ---
submethod BUILD(
    :$v-r!,    :$v-b!,    :$v-swpd!, :$v-free!,  :$v-buff!, :$v-cache!,
    :$v-si!,   :$v-so!,   :$v-bi!,   :$v-bo!,    :$v-in!,   :$v-cs!,
    :$v-us!,   :$v-sy!,   :$v-id!,   :$v-wa!,    :$v-st!,   :$v-gu!,
    :$v-date!, :$v-time!
) {
    # Parse with explicit helpers, yielding clear errors.
    $.v-r     = self!to-int($v-r,    'v-r');
    $.v-b     = self!to-int($v-b,    'v-b');
    $.v-swpd  = self!to-int($v-swpd, 'v-swpd');
    $.v-free  = self!to-int($v-free, 'v-free');
    $.v-buff  = self!to-int($v-buff, 'v-buff');
    $.v-cache = self!to-int($v-cache,'v-cache');
    $.v-si    = self!to-int($v-si,   'v-si');
    $.v-so    = self!to-int($v-so,   'v-so');
    $.v-bi    = self!to-int($v-bi,   'v-bi');
    $.v-bo    = self!to-int($v-bo,   'v-bo');
    $.v-in    = self!to-int($v-in,   'v-in');
    $.v-cs    = self!to-int($v-cs,   'v-cs');

    # Percentages: parse to Int and optionally enforce 0..100
    $.v-us    = self!to-percent($v-us, 'v-us');
    $.v-sy    = self!to-percent($v-sy, 'v-sy');
    $.v-id    = self!to-percent($v-id, 'v-id');
    $.v-wa    = self!to-percent($v-wa, 'v-wa');
    $.v-st    = self!to-percent($v-st, 'v-st');
    $.v-gu    = self!to-percent($v-gu, 'v-gu');  # or optional if not always present

    # Date/Time: keep raw strings or parse to types if desired
    $.v-date  = self!to-str($v-date, 'v-date');
    $.v-time  = self!to-str($v-time, 'v-time');
}

# --- TWEAK: cross-field validations after attributes are set ---
submethod TWEAK() {
    # Example: percentages shouldn’t exceed 100 when summed (tweak to your semantics)
    my $sum = $.v-us + $.v-sy + $.v-id + $.v-wa + $.v-st + $.v-gu;
    # Comment this out if vmstat columns don’t strictly partition 100% in your environment.
    # die "Invalid percentage sum ($sum) > 100" if $sum > 100;

    # Example: non-negative invariants
    for <v-r v-b v-swpd v-free v-buff v-cache v-si v-so v-bi v-bo v-in v-cs> -> $name {
        my $val = self."$name"();
        die "$name must be >= 0, got $val" if $val < 0;
    }
}

# --- Private helpers for parsing/coercion with good errors ---
method !to-int(Any $x, Str $name --> Int) {
    # Accept Int/Numeric/Str; reject undefined or malformed.
    die "$name is required" unless $x.defined;
    my $n = do given $x {
        when Int     { $_ }
        when Numeric { $_.Int }
        default      { $_.Str.Numeric.Int }
    }
    # Optional: check that input was actually numeric (Numeric on Str returns 0 for non-numeric)
    # A stricter check:
    my $s = $x.Str;
    die "$name must be integer-like, got '$s'"
        unless $s ~~ /^ \s* [\+\-]? \d+ \s* $/;
    $n
}

method !to-percent(Any $x, Str $name --> Int) {
    my $n = self!to-int($x, $name);
    die "$name must be within 0..100, got $n" unless 0 <= $n <= 100;
    $n
}

method !to-str(Any $x, Str $name --> Str) {
    die "$name is required" unless $x.defined;
    $x.Str
}

# --- Convenience: construct from a whitespace-delimited vmstat line ---
method from-line(Str:D $line) {
    my @keys = <v-r v-b v-swpd v-free v-buff v-cache v-si v-so v-bi v-bo v-in v-cs v-us v-sy v-id v-wa v-st v-gu v-date v-time>;
    my @vals = $line.words;

    die "Expected {@keys.elems} fields, got {@vals.elems} in line: $line"
        unless @vals.elems == @keys.elems;

    self.new(|(@keys Z=> @vals).Hash);
}


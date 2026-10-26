#!/usr/bin/env nu
# Usage: bench/kept.nu [--runs N] [--reads R...] [BIN]
# What keeping an applied nixpkgs saves a library host: `outPath` of R
# packages (1, 10, 30 and 300 by default; past 30 the list repeats), one
# read per call, with nixpkgs applied once and kept, applied again in the
# same evaluator for each read, or applied in a new session for each read
# (examples/kept_bench.rs). Wall and peak RSS of the run with the median
# wall of N (default 5). Run it on an idle box.

const ATTRS = [
  hello curl git jq ripgrep fd tree htop tmux zlib
  openssl sqlite libxml2 libffi ncurses readline xz zstd bzip2 gnutar
  gzip findutils gnused gawk gnugrep diffutils patch file coreutils bash
]

def measure [bin: string, args: list<string>]: nothing -> record {
  let r = (^/usr/bin/env time -f "TIME %e %M" $bin ...$args | complete)
  let line = ($r.stderr | lines | where $it starts-with "TIME " | last)
  let f = ($line | split row " ")
  let out = ($r.stdout | str trim | split row " ")
  {
    wall: ($f | get 1 | into float)
    rss_mb: (($f | get 2 | into int) / 1024 | math round)
    failed: ($out | get 2 | into int)
    session_mb: ($out | get 4 | into int)
    rc: $r.exit_code
  }
}

def median-run [bin: string, args: list<string>, runs: int]: nothing -> record {
  let rs = (1..$runs | each {|_| measure $bin $args })
  let sorted = ($rs | sort-by wall)
  $sorted | get (($runs - 1) // 2)
}

def main [
  --runs: int = 5
  --reads: list<int> = [1 10 30 300]
  bin: string = "./target/release/examples/kept_bench"
] {
  cd ($env.FILE_PWD | path join "..")
  let np = ($env.NIXPKGS? | default "/nix/store/ah3pxz4xa52hd6vhbx17vcsci0b8bmbr-source")
  print "| mode | reads | wall s | peak RSS MB | largest session MB | failed reads |"
  print "|---|---|---|---|---|---|"
  for n in $reads {
    let attrs = (0..<$n | each {|i| $ATTRS | get ($i mod ($ATTRS | length)) })
    for mode in [kept again fresh] {
      let r = (median-run $bin [$mode $np ...$attrs] $runs)
      print $"| ($mode) | ($n) | ($r.wall) | ($r.rss_mb) | ($r.session_mb) | ($r.failed) |"
    }
  }
}

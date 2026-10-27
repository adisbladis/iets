#!/usr/bin/env nu
# Usage: bench/ab.nu [--runs N] [--skip-outpaths] BEFORE AFTER
# Interleaved A/B of two iets binaries: one hyperfine invocation per
# workload (`--warmup 1 --runs N`, default 7) so the runs alternate, on
# nixos-server at 1 and 4 workers and pure from a checkout at 1, nixos-minimal at 1 worker, and outpaths
# chunk 0 of 8 (`eval-jobs --no-instantiate`) at 1 worker. Prints the
# median wall, mean user and sys, the peak RSS of one extra timed run, and
# the wall change. Run it on an idle box.

def rss-mb [bin: string, args: list<string>]: nothing -> int {
  let r = (^/usr/bin/env time -f "TIME %M" $bin ...$args | complete)
  let line = ($r.stderr | lines | where $it starts-with "TIME " | last)
  ($line | split row " " | get 1 | into int) / 1024 | math round
}

# Everything that can fail or be interrupted once the temporary files exist,
# so `main` removes them either way.
def measure [
  before: string, after: string, runs: int, skip_outpaths: bool, np: string,
  server: string, minimal: string, outpaths: string, checkout: string
]: nothing -> list<record> {
  ^cp -r $"($np)/." $checkout
  ^chmod -R u+w $checkout
  "" | save -f $"($checkout)/iets.toml"
  open --raw $server | str replace "<nixpkgs>" "./." | save -f $"($checkout)/bench.nix"
  $"import ($np)/ci/eval/outpaths.nix { path = ($np); systems = [ \"x86_64-linux\" ]; includeBroken = false; }\n" | save -f $outpaths
  mut cases = [
    { name: "nixos-server w1", args: [eval --impure --strict --no-tracing-cache --workers "1" -I $"nixpkgs=($np)" $server] }
    { name: "nixos-server w4", args: [eval --impure --strict --no-tracing-cache --workers "4" -I $"nixpkgs=($np)" $server] }
    { name: "nixos-server pure w1", args: [eval --strict --no-tracing-cache --workers "1" --lint "absolute-path-in-drv=off" $"($checkout)/bench.nix"] }
    { name: "nixos-minimal w1", args: [eval --impure --strict --no-tracing-cache --workers "1" -I $"nixpkgs=($np)" $minimal] }
  ]
  if not $skip_outpaths {
    $cases = ($cases | append { name: "outpaths 0/8 w1", args: [eval-jobs --impure --no-instantiate --workers "1" --chunks "8" --chunk "0" -I $"nixpkgs=($np)" $outpaths] })
  }
  mut rows = []
  for c in $cases {
    let json = (mktemp --tmpdir ab.XXXXXX)
    let a = ([$before] ++ $c.args | str join " ")
    let b = ([$after] ++ $c.args | str join " ")
    ^hyperfine -N --warmup 1 --runs $runs --export-json $json $a $b out+err> /dev/null
    let res = (open --raw $json | from json | get results)
    rm -f $json
    let ra = ($res | get 0)
    let rb = ($res | get 1)
    let rss_a = (rss-mb $before $c.args)
    let rss_b = (rss-mb $after $c.args)
    $rows = ($rows | append {
      workload: $c.name
      before_wall: ($ra.median | math round --precision 3)
      after_wall: ($rb.median | math round --precision 3)
      change: $"(((($rb.median - $ra.median) / $ra.median) * 100) | math round --precision 1)%"
      before_user: ($ra.user | math round --precision 2)
      after_user: ($rb.user | math round --precision 2)
      before_sys: ($ra.system | math round --precision 2)
      after_sys: ($rb.system | math round --precision 2)
      before_rss: $rss_a
      after_rss: $rss_b
    })
  }
  $rows
}

def main [
  --runs: int = 7
  --skip-outpaths
  before: string
  after: string
] {
  cd ($env.FILE_PWD | path join "..")
  let np = ($env.NIXPKGS? | default "/nix/store/ah3pxz4xa52hd6vhbx17vcsci0b8bmbr-source")
  let server = $"($env.PWD)/bench/workloads/nixos-server.nix"
  let minimal = $"($env.PWD)/bench/workloads/nixos-minimal.nix"
  let outpaths = (mktemp --tmpdir outpaths.XXXXXX)
  # a checkout with a project marker: every file a pure eval reads is a
  # project path, which `store_hooks::admits` checks
  let checkout = (mktemp -d --tmpdir checkout.XXXXXX)
  let rows = try {
    measure $before $after $runs $skip_outpaths $np $server $minimal $outpaths $checkout
  } catch {|e|
    rm -rf $outpaths $checkout
    error make {msg: $e.msg}
  }
  rm -rf $outpaths $checkout
  print "| workload | wall before | wall after | change | user before | user after | sys before | sys after | RSS before | RSS after |"
  print "|---|---|---|---|---|---|---|---|---|---|"
  for r in $rows {
    print $"| ($r.workload) | ($r.before_wall) | ($r.after_wall) | ($r.change) | ($r.before_user) | ($r.after_user) | ($r.before_sys) | ($r.after_sys) | ($r.before_rss) | ($r.after_rss) |"
  }
}

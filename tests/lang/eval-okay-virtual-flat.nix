let
  f = builtins.path {
    path = ./data;
    recursive = false;
  };
in
[
  (builtins.readFile f)
  (builtins.pathExists f)
  (builtins.hashFile "sha256" f)
]

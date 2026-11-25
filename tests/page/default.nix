# The dashboard page in a real browser (headless Chromium) against mock.py's API: no horizontal overflow at
# any width from 900 to 1400 px with classic scrollbars, on Nodes and on an app's VMs tab (sweep.py); Add
# node's steps, the Edges' Listen column and Apps' not-ready checklist (ui.py).
{ pkgs, lib }:
pkgs.runCommand "flyyy-page-overflow" {
  nativeBuildInputs = [ (pkgs.python3.withPackages (ps: [ ps.playwright ])) ];
  FONTCONFIG_FILE = pkgs.makeFontsConf { fontDirectories = [ pkgs.dejavu_fonts ]; };
} ''
  export HOME=$TMPDIR
  python3 ${./mock.py} ${../../proxy/src/dashboard.html} 4567 &
  until python3 -c "import urllib.request; urllib.request.urlopen('http://127.0.0.1:4567/config.json')" 2> /dev/null; do sleep 0.2; done
  python3 ${./sweep.py} http://127.0.0.1:4567 ${lib.getExe pkgs.chromium} | tee $out
  python3 ${./ui.py} http://127.0.0.1:4567 ${lib.getExe pkgs.chromium} | tee -a $out
  kill %1
''

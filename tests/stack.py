# The dev stack (`nix run .#stack`, replacing up.sh/down.sh): boot and seed run by `test_script()`, then
# use the prelude's helpers from the driver's REPL; leaving the REPL tears everything down.
print("""
stack up: whoami x6, echo x4. From this REPL, for example:
  print(fly("ls"))                                    # the flyyy CLI, in the control container
  print(edges.succeed("curl -s https://whoami.flyyy.localhost/"))   # through both edges (TLS on 127.0.0.1:443 there)
  print(bh("worker1", live("whoami")[0][1]))                 # one machine through worker1's backhaul
  print(journal(worker1, "agent"))                    # any unit's log; also worker1.shell_interact()
  fly(f"deploy blue --image {image('flyyy/testapp')} --port 8080 --count 3")
""")

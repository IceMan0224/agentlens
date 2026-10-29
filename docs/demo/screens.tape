# Screenshots of the shorter views for the README (see demo.tape).

Output target/demo-screens.gif
Set Shell "bash"
Set FontFamily "DejaVu Sans Mono"
Set Padding 12
Set Theme "Catppuccin Mocha"
Set FontSize 14
Set Width 1800
Set Height 330

Hide
Type "./target/release/agentlens --replay crates/core/tests/fixtures/sample.json"
Enter
Sleep 1.5s
Show
Type "2"
Sleep 1s
Screenshot docs/images/cleanup.png
Sleep 300ms
Type "3"
Sleep 1s
Screenshot docs/images/network.png
Sleep 300ms
Type "4"
Sleep 1s
Screenshot docs/images/ports.png
Sleep 300ms
Type "6"
Sleep 1s
Screenshot docs/images/settings.png
Sleep 300ms

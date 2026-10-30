# The Sessions view for the README, in English and in Korean (see demo.tape).

Output target/demo-sessions.gif
Set Shell "bash"
Set FontFamily "DejaVu Sans Mono"
Set Padding 12
Set Theme "Catppuccin Mocha"
Set FontSize 14
Set Width 1700
Set Height 780

Hide
Type "./target/release/agentlens --replay crates/core/tests/fixtures/sample.json"
Enter
Sleep 1.5s
Show
Sleep 500ms
Screenshot docs/images/sessions.png
Type "q"
Hide
Type "XDG_CONFIG_HOME=docs/demo/ko ./target/release/agentlens --replay crates/core/tests/fixtures/sample.json"
Enter
Sleep 1.5s
Show
Sleep 500ms
Screenshot docs/images/sessions-ko.png
Sleep 300ms

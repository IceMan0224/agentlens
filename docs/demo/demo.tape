# The README's animation. From the repository root, after `cargo build --release`:
#   vhs docs/demo/demo.tape
# Replays the made-up sample snapshot read-only: nothing of the recording machine is shown.

Output docs/images/demo.gif
Set Shell "bash"
Set FontFamily "DejaVu Sans Mono"
Set Padding 12
Set Theme "Catppuccin Mocha"
Set FontSize 14
Set Width 1700
Set Height 760
Set TypingSpeed 120ms

Hide
Type "./target/release/agentlens --replay crates/core/tests/fixtures/sample.json"
Enter
Sleep 1.5s
Show
Sleep 2.5s
Down 5
Sleep 600ms
Right
Sleep 1.5s
Enter
Sleep 2.5s
Enter
Sleep 500ms
Type "2"
Sleep 3s
Type "x"
Sleep 2.5s
Type "n"
Sleep 800ms
Type "4"
Sleep 2.5s
Type "3"
Sleep 2s
Type "6"
Sleep 2s
Type "1"
Sleep 1s

Output demo/basic.gif

Require sey

Set Shell "bash"
Set FontSize 16
Set Width 1300
Set Height 1150
Set Padding 20
Set Theme "Catppuccin Mocha"
Set TypingSpeed 40ms

Hide
Type "cd $SEY_DEMO_SCRATCH && clear" Enter
Show

Type `sey -w interpolate substitute .`
Sleep 800ms
Enter
Sleep 2.5s
Type "q"
Sleep 1s

Type `sey -w interpolate substitute . -y`
Sleep 800ms
Enter
Sleep 2s

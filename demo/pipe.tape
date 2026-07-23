Output demo/pipe.gif

Require sey

Set Shell "bash"
Set FontSize 16
Set Width 1300
Set Height 1000
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
Type "n"
Sleep 500ms
Enter
Sleep 1s

Type `sey -w interpolate substitute . | cat`
Sleep 800ms
Enter
Sleep 3s

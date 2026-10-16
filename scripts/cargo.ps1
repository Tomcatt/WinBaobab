$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
if ([Environment]::ProcessorCount -ge 32) { (Get-Process -Id $PID).ProcessorAffinity = [IntPtr][long]0xFFFF0000L }

# PowerShell swallows the first bare `--` when calling a script, so
# `cargo.ps1 test -- --ignored` would reach cargo as `test --ignored`.
# Recover its position from the invocation text and put it back.
$cargoArgs = @($args)
try {
    $parseErrors = $null
    $tokens = [System.Management.Automation.PSParser]::Tokenize($MyInvocation.Line, [ref]$parseErrors)
    $stop = @('StatementSeparator', 'NewLine', 'LineContinuation')
    for ($start = 0; $start -lt $tokens.Count; $start++) {
        if ($tokens[$start].Content -notlike '*cargo.ps1') { continue }
        # Collect this call's argument tokens (up to ; | newline or a redirection).
        $argTokens = @()
        for ($i = $start + 1; $i -lt $tokens.Count; $i++) {
            $tok = $tokens[$i]
            if ($stop -contains [string]$tok.Type -or $tok.Content -eq '|' -or $tok.Content -match '^\d*>') { break }
            $argTokens += $tok
        }
        $dd = -1
        for ($i = 0; $i -lt $argTokens.Count; $i++) {
            if ($argTokens[$i].Type -eq 'Operator' -and $argTokens[$i].Content -eq '--') { $dd = $i; break }
        }
        # Only trust the recovery when token and argument counts line up.
        if ($dd -ge 0 -and $argTokens.Count -eq $args.Count + 1) {
            $list = [System.Collections.Generic.List[object]]::new()
            $list.AddRange([object[]]$args)
            $list.Insert($dd, '--')
            $cargoArgs = $list.ToArray()
            break
        }
    }
} catch { $cargoArgs = @($args) }

& cargo @cargoArgs
exit $LASTEXITCODE

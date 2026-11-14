# Claude Code subagent rows on Windows: the PowerShell twin of
# subagent-statusline.sh (name, the model that subagent runs on, effort, what
# it is doing, how long it has run, and its tokens, cost and context use,
# right-aligned to `columns`). It reads the same JSON on stdin and prints the
# same bytes as the original's `jq -c` program, without jq, on Windows
# PowerShell 5.1 and PowerShell 7. Connecto runs it with
# `powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File`.
#
# What it copies of jq: one compact JSON line `{"id":...,"content":"..."}`
# per entry of `.tasks`, with jq's string escapes (\u001b for ESC, other text
# as UTF-8); widths count terminal columns of code points as the original's
# `cpw` does; `a // b` takes b when a is
# null, false or an error. An input that is not JSON prints nothing; an error
# in a row ends the rows of that input (the rows before it stay). Never fails:
# the exit code is 0.
#
# Keep this file pure ASCII (Windows PowerShell 5.1 reads a file without a
# BOM in the ANSI code page): non-ASCII output is built from code points.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$E = [string][char]27
$Dot = [string][char]0x00B7
$Ellipsis = [string][char]0x2026
$Sep = ' ' + $Dot + ' '
$Inv = [Globalization.CultureInfo]::InvariantCulture
$DictType = [Collections.Specialized.OrderedDictionary]
$ListType = [Collections.Generic.List[object]]

# ---- JSON, the way jq reads it ---------------------------------------------

# A token, followed by whatever may end it. Literals and numbers are as
# lenient as jq's (`01`, `.5`, `+1`).
$TokRe = [regex]::new('\G[ \t\n\r]*("(?:[^"\\]|\\["\\/bfnrt]|\\u[0-9a-fA-F]{4})*"|[{}\[\]:,]|' +
    '(?:true|false|null|[+-]?(?:[0-9]+\.?[0-9]*|\.[0-9]+)(?:[eE][+-]?[0-9]+)?)(?=[ \t\n\r{}\[\]:,"]|$))')
$EscRe = [regex]::new('\\(?:u([0-9a-fA-F]{4})|(.))')

function Read-Stdin {
    $reader = [IO.StreamReader]::new([Console]::OpenStandardInput(), [Text.UTF8Encoding]::new($false), $false)
    $text = $reader.ReadToEnd()
    if ($text.Length -gt 0 -and $text[0] -eq [char]0xFEFF) { $text = $text.Substring(1) }
    $text
}

function Split-Tokens([string]$text) {
    $list = [Collections.Generic.List[string]]::new()
    $pos = 0
    while ($true) {
        $m = $TokRe.Match($text, $pos)
        if (-not $m.Success) { break }
        $list.Add($m.Groups[1].Value)
        $pos = $m.Index + $m.Length
    }
    $script:tk = $list.ToArray()
    $script:n = $list.Count
    $script:p = 0
}

function Get-Unquoted([string]$t) {
    $inner = $t.Substring(1, $t.Length - 2)
    if ($inner.IndexOf([char]'\') -lt 0) { return $inner }
    $EscRe.Replace($inner, {
            param($m)
            if ($m.Groups[1].Success) { return [string][char][Convert]::ToInt32($m.Groups[1].Value, 16) }
            switch -CaseSensitive ($m.Groups[2].Value) {
                'b' { return [string][char]8 }
                'f' { return [string][char]12 }
                'n' { return "`n" }
                'r' { return "`r" }
                't' { return "`t" }
                default { return $m.Groups[2].Value }
            }
        })
}

function ConvertTo-Number([string]$t) {
    try { return [double]::Parse($t, [Globalization.NumberStyles]::Float, $Inv) }
    catch { if ($t[0] -eq [char]'-') { return - [double]::MaxValue } else { return [double]::MaxValue } }
}

# The next value into $script:v; throws when the input is not JSON there.
function Read-Value {
    if ($script:p -ge $script:n) { throw 'end of input' }
    $t = $script:tk[$script:p]
    $script:p++
    $c = $t[0]
    if ($c -eq [char]'{') {
        $o = [Collections.Specialized.OrderedDictionary]::new()
        if ($script:tk[$script:p] -ceq '}') { $script:p++; $script:v = $o; return }
        while ($true) {
            $k = $script:tk[$script:p]
            if ($null -eq $k -or $k[0] -ne [char]'"' -or $script:tk[$script:p + 1] -cne ':') { throw 'object' }
            $script:p += 2
            Read-Value
            $o[[object](Get-Unquoted $k)] = $script:v
            $t = $script:tk[$script:p]
            $script:p++
            if ($t -ceq ',') { continue }
            if ($t -ceq '}') { break }
            throw 'object'
        }
        $script:v = $o
        return
    }
    if ($c -eq [char]'[') {
        $a = [Collections.Generic.List[object]]::new()
        if ($script:tk[$script:p] -ceq ']') { $script:p++; $script:v = $a; return }
        while ($true) {
            Read-Value
            $a.Add($script:v)
            $t = $script:tk[$script:p]
            $script:p++
            if ($t -ceq ',') { continue }
            if ($t -ceq ']') { break }
            throw 'array'
        }
        $script:v = $a
        return
    }
    if ($c -eq [char]'"') { $script:v = Get-Unquoted $t; return }
    if ($t -ceq 'true') { $script:v = $true; return }
    if ($t -ceq 'false') { $script:v = $false; return }
    if ($t -ceq 'null') { $script:v = $null; return }
    if ('}]:,'.IndexOf($c) -ge 0) { throw 'value' }
    $script:v = ConvertTo-Number $t
}

# ---- jq ----------------------------------------------------------------------

# `.key` of a value: null stays null, anything but an object is an error.
function Get-Key($value, [string]$key) {
    if ($null -eq $value) { return $null }
    if ($value -is $DictType) { return , $value[[object]$key] }
    throw 'cannot index'
}

# `.key` on the left of `//`, where an error counts as null.
function Get-Soft($value, [string]$key) {
    if ($value -isnot $DictType) { return $null }
    , $value[[object]$key]
}

# `a // b`: b when a is null or false.
function Get-Or($a, $b) {
    if ($null -eq $a -or ($a -is [bool] -and -not $a)) { return , $b }
    , $a
}

function Format-Number([double]$d) {
    if ([double]::IsNaN($d)) { return 'null' }
    if ([double]::IsInfinity($d)) { if ($d -gt 0) { $d = [double]::MaxValue } else { $d = - [double]::MaxValue } }
    if ([Math]::Floor($d) -eq $d -and [Math]::Abs($d) -lt 1e17) { return ([long]$d).ToString($Inv) }
    $r = $d.ToString('R', $Inv)
    if ([double]::Parse($r, $Inv) -ne $d) { $r = $d.ToString('G17', $Inv) }
    $r.Replace('E', 'e')
}

function ConvertTo-JsonText($value) {
    if ($null -eq $value) { return 'null' }
    if ($value -is [bool]) { if ($value) { return 'true' } else { return 'false' } }
    if ($value -is [double]) { return Format-Number $value }
    if ($value -is [string]) {
        $s = [regex]::Replace($value, '["\\\x00-\x1f\x7f]', {
                param($m)
                switch ([int][char]$m.Value) {
                    34 { return '\"' }
                    92 { return '\\' }
                    8 { return '\b' }
                    9 { return '\t' }
                    10 { return '\n' }
                    12 { return '\f' }
                    13 { return '\r' }
                    default { return '\u' + ([int][char]$m.Value).ToString('x4') }
                }
            })
        return '"' + $s + '"'
    }
    if ($value -is $ListType) {
        $parts = foreach ($x in $value) { ConvertTo-JsonText $x }
        return '[' + ($parts -join ',') + ']'
    }
    $parts = foreach ($k in $value.Keys) { (ConvertTo-JsonText ([string]$k)) + ':' + (ConvertTo-JsonText $value[$k]) }
    '{' + ($parts -join ',') + '}'
}

# jq's `tostring`.
function ConvertTo-Text($value) {
    if ($value -is [string]) { return $value }
    ConvertTo-JsonText $value
}

function Get-String($value) {
    if ($value -is [string]) { return $value }
    throw 'not a string'
}

function Get-Number($value) {
    if ($value -is [double]) { return $value }
    throw 'not a number'
}

function Get-Paint([string]$s, [string]$code) {
    $E + '[' + $code + 'm' + $s + $E + '[0m'
}

# short: claude-opus-5-5 -> Opus 5.5; other ids are shown as they are.
function Get-Short([string]$id) {
    $id = ([regex]'\[.*\]$').Replace($id, '', 1)
    $id = ([regex]'-[0-9]{8}$').Replace($id, '', 1)
    if (-not [regex]::IsMatch($id, '^claude-[a-z]+-[0-9]')) { return $id }
    $p = ([regex]'^claude-').Replace($id, '', 1).Split([char]'-')
    # ascii_upcase: only a-z change.
    $c = $p[0][0]
    if ($c -ge [char]'a' -and $c -le [char]'z') { $c = [char]([int]$c - 32) }
    $first = [string]$c + $p[0].Substring(1)
    $first + ' ' + [string]::Join('.', $p, 1, $p.Length - 1)
}

# human: 1.2M from a million on, else 123k (floored).
function Get-Human([double]$x) {
    if ($x -ge 1000000) { return (Format-Number ([Math]::Floor($x / 100000) / 10)) + 'M' }
    (Format-Number ([Math]::Floor($x / 1000))) + 'k'
}

# jq's `a % b`: both cut to whole numbers, the sign of a.
function Get-Mod([double]$a, [double]$b) {
    [double]([long][Math]::Truncate($a) % [long][Math]::Truncate($b))
}

# Two digits at least: 5 -> 05.
function Get-Pad2([double]$x) {
    $s = Format-Number $x
    if ($s.Length -lt 2) { return '0' + $s }
    $s
}

# money: dollars and cents from a number of dollars ($1.23).
function Get-Money([double]$x) {
    $c = [Math]::Round($x * 100, [MidpointRounding]::AwayFromZero)
    $cents = Format-Number (Get-Mod $c 100)
    if ($cents.Length -lt 2) { $cents = '0' + $cents }
    '$' + (Format-Number ([Math]::Floor($c / 100))) + '.' + $cents
}

# elapsed: whole seconds to a compact span: 45s, 12m 34s, 1h 05m 09s.
function Get-Elapsed([double]$x) {
    if ($x -lt 60) { return (Format-Number $x) + 's' }
    if ($x -lt 3600) { return (Format-Number ([Math]::Floor($x / 60))) + 'm ' + (Get-Pad2 (Get-Mod $x 60)) + 's' }
    (Format-Number ([Math]::Floor($x / 3600))) + 'h ' + (Get-Pad2 ([Math]::Floor((Get-Mod $x 3600) / 60))) + 'm ' + (Get-Pad2 (Get-Mod $x 60)) + 's'
}

# jq's `explode`: the code points of a string.
function Get-CodePoints([string]$s) {
    $list = [Collections.Generic.List[int]]::new()
    $i = 0
    while ($i -lt $s.Length) {
        if ([char]::IsHighSurrogate($s[$i]) -and $i + 1 -lt $s.Length -and [char]::IsLowSurrogate($s[$i + 1])) {
            $list.Add([char]::ConvertToUtf32($s[$i], $s[$i + 1]))
            $i += 2
        } else {
            $list.Add([int]$s[$i])
            $i++
        }
    }
    , $list.ToArray()
}

# jq's `implode`.
function ConvertFrom-CodePoints($cps) {
    $sb = [Text.StringBuilder]::new()
    foreach ($c in $cps) {
        if ($c -ge 0x10000) { [void]$sb.Append([char]::ConvertFromUtf32($c)) } else { [void]$sb.Append([char]$c) }
    }
    $sb.ToString()
}

# clean: control characters (C0, DEL and C1) dropped.
function Get-Clean([string]$s) {
    $kept = foreach ($c in (Get-CodePoints $s)) { if ($c -ge 32 -and $c -ne 127 -and ($c -lt 128 -or $c -gt 159)) { $c } }
    ConvertFrom-CodePoints $kept
}

# cpw: terminal columns of one code point (combining marks none, East Asian
# wide and emoji two, everything else one), as the original counts them.
function Get-Cpw([int]$c) {
    if ($c -eq 8203 -or $c -eq 8205 -or ($c -ge 768 -and $c -le 879) -or ($c -ge 65024 -and $c -le 65039)) { return 0 }
    if (($c -ge 4352 -and $c -le 4447) -or ($c -ge 11904 -and $c -le 42191) -or ($c -ge 44032 -and $c -le 55203) -or
        ($c -ge 63744 -and $c -le 64255) -or ($c -ge 65072 -and $c -le 65103) -or ($c -ge 65280 -and $c -le 65376) -or
        ($c -ge 65504 -and $c -le 65510) -or ($c -ge 127744 -and $c -le 129791) -or ($c -ge 131072 -and $c -le 262141)) { return 2 }
    1
}

# cw: the columns of a string.
function Get-Cw([string]$s) {
    $n = 0
    foreach ($c in (Get-CodePoints $s)) { $n += Get-Cpw $c }
    $n
}

# fit: the longest start that fits in $w columns, with an ellipsis when cut.
function Get-Fit([string]$s, [double]$w) {
    if ((Get-Cw $s) -le $w) { return $s }
    if ($w -lt 1) { return '' }
    $out = [Collections.Generic.List[int]]::new()
    $n = 0
    foreach ($c in (Get-CodePoints $s)) {
        $cw = Get-Cpw $c
        if ($n + $cw -le $w - 1) { $out.Add($c); $n += $cw } else { break }
    }
    (([regex]' +$').Replace((ConvertFrom-CodePoints $out), '', 1)) + $Ellipsis
}

# Segments: a list of (text, colour or null).
function New-Segs { , [Collections.Generic.List[object]]::new() }
function Add-Seg($segs, [string]$text, $colour) { $segs.Add([object[]]@($text, $colour)) }
function Join-Segs($a, $b) {
    $l = [Collections.Generic.List[object]]::new()
    foreach ($s in $a) { $l.Add($s) }
    foreach ($s in $b) { $l.Add($s) }
    , $l
}
function Get-Plain($segs) {
    $sb = [Text.StringBuilder]::new()
    foreach ($s in $segs) { [void]$sb.Append($s[0]) }
    $sb.ToString()
}
function Get-Painted($segs) {
    $sb = [Text.StringBuilder]::new()
    foreach ($s in $segs) {
        if ($null -ne $s[1]) { [void]$sb.Append((Get-Paint $s[0] $s[1])) } else { [void]$sb.Append($s[0]) }
    }
    $sb.ToString()
}
function Get-Width($segs) { Get-Cw (Get-Plain $segs) }
function Get-Gap($segs) { if ($segs.Count -gt 0) { 2 } else { 0 } }
function Get-Spaces([double]$n) { if ($n -gt 0) { ' ' * [int]$n } else { '' } }

# The head: name, then model and effort when asked for.
function Get-HeadSegs([string]$name, [string]$model, [string]$effort, [bool]$wm, [bool]$we) {
    $h = New-Segs
    Add-Seg $h $name '36'
    if ($wm -and $model -cne '') { Add-Seg $h $Sep '90'; Add-Seg $h $model '35' }
    if ($we -and $effort -cne '') { Add-Seg $h $Sep '90'; Add-Seg $h $effort '90' }
    , $h
}

# The first head that fits in $avail columns (full, without effort, name
# only), or null.
function Get-FittingHead([string]$name, [string]$model, [string]$effort, [double]$avail) {
    foreach ($ask in @(@($true, $true), @($true, $false), @($false, $false))) {
        $h = Get-HeadSegs $name $model $effort $ask[0] $ask[1]
        if ((Get-Width $h) -le $avail) { return , $h }
    }
    $null
}

# The left side in $avail columns: the head, then the label in what is left
# (dropped under 8 columns); a cut name when not even the bare head fits.
function Get-Left([string]$name, [string]$model, [string]$effort, [string]$label, [double]$avail) {
    $h = Get-FittingHead $name $model $effort $avail
    if ($null -eq $h) {
        $l = New-Segs
        Add-Seg $l (Get-Fit $name $avail) '36'
        return , $l
    }
    $room = $avail - (Get-Width $h) - 3
    if ($label -cne '' -and $room -ge 8) {
        Add-Seg $h $Sep '90'
        Add-Seg $h (Get-Fit $label $room) $null
    }
    , $h
}

# The rows of one input value, each appended to $rows as it is made.
function Add-Rows($in, [Text.StringBuilder]$rows) {
    # A usable width is a whole number of at least 1, clamped to 1000.
    $raw = Get-Key $in 'columns'
    $W = $null
    if ($raw -is [double] -and $raw -eq [Math]::Floor($raw) -and $raw -ge 1) {
        if ($raw -gt 1000) { $W = 1000.0 } else { $W = $raw }
    }
    $tasks = $null
    try { $tasks = Get-Key $in 'tasks' } catch { return }
    if ($tasks -is $ListType) { $items = $tasks.ToArray() }
    elseif ($tasks -is $DictType) { $items = @($tasks.Values) }
    else { return }
    $dot = ' ' + (Get-Paint $Dot '90') + ' '
    foreach ($task in $items) {
        $id = Get-Key $task 'id'
        $name = Get-Clean (Get-String (Get-Or (Get-Soft $task 'name') (Get-Or (Get-Soft $task 'agentType') 'agent')))
        $model = Get-Short (Get-Clean (Get-String (Get-Or (Get-Soft $task 'model') '')))
        $effort = Get-Clean (ConvertTo-Text (Get-Or (Get-Soft $task 'effort') ''))
        $label = Get-Clean (Get-String (Get-Or (Get-Soft $task 'label') (Get-Or (Get-Soft $task 'description') '')))
        $tokenCount = Get-Number (Get-Or (Get-Soft $task 'tokenCount') 0.0)
        $tokens = Get-Human $tokenCount
        $used = ''
        $size = Get-Or (Get-Soft $task 'contextWindowSize') 0.0
        if ($size -is [double]) { $positive = $size -gt 0 } else { $positive = -not ($size -is [bool]) }
        if ($positive) {
            $used = ' ' + (Format-Number ([Math]::Floor($tokenCount * 100 / (Get-Number (Get-Key $task 'contextWindowSize'))))) + '%'
        }
        $cost = $null
        try { $cost = Get-Key (Get-Key $task 'cost') 'total_cost_usd' } catch { $cost = $null }
        # The running time: a whole, positive start in ms, a task that is
        # running (or says nothing), not in the future, under 30 days.
        $start = Get-Key $task 'startTime'
        $status = Get-Key $task 'status'
        $age = $null
        if ($start -is [double] -and $start -gt 0 -and $start -eq [Math]::Floor($start) -and
            ($null -eq $status -or ($status -is [string] -and $status -ceq 'running'))) {
            $nowMs = ([DateTime]::UtcNow - [DateTime]::new(1970, 1, 1, 0, 0, 0, [DateTimeKind]::Utc)).TotalMilliseconds
            $span = [Math]::Floor(($nowMs - $start) / 1000)
            if ($span -ge 0 -and $span -lt 2592000) { $age = $span }
        }
        $time = New-Segs
        if ($null -ne $age) { Add-Seg $time (Get-Elapsed $age) '90'; Add-Seg $time '  ' $null }
        $counts = New-Segs
        Add-Seg $counts ($tokens + $used) '33'
        if ($cost -is [double]) { Add-Seg $counts $Sep '90'; Add-Seg $counts (Get-Money $cost) '33' }
        $full = Join-Segs $time $counts
        $pct = New-Segs
        if ($used -cne '') { Add-Seg $pct $used.Substring(1) '33' }
        if ($null -ne $W) {
            $tier = $null
            foreach ($candidate in @($full, $counts, $pct)) {
                if ($null -ne (Get-FittingHead $name $model $effort ($W - (Get-Width $candidate) - (Get-Gap $candidate)))) {
                    $tier = $candidate
                    break
                }
            }
            if ($null -ne $tier) {
                $avail = $W - (Get-Width $tier) - (Get-Gap $tier)
                $l = Get-Left $name $model $effort $label $avail
                $content = (Get-Painted $l) + (Get-Spaces ($W - (Get-Width $l) - (Get-Width $tier))) + (Get-Painted $tier)
            } else {
                # Not even the bare name fits beside the block: keep the
                # percentage (cut on a row narrower than it), cut the name.
                if ((Get-Width $pct) -le $W) { $r = $pct } else {
                    $r = New-Segs
                    Add-Seg $r (Get-Fit (Get-Plain $pct) $W) '33'
                }
                $avail = $W - (Get-Width $r) - (Get-Gap $r)
                $n = Get-Fit $name $avail
                $content = (Get-Paint $n '36') + (Get-Spaces ($W - (Get-Cw $n) - (Get-Width $r))) + (Get-Painted $r)
            }
        } else {
            $l = Get-Left $name $model $effort $label (120 - (Get-Width $full) - 3)
            $content = (Get-Painted $l) + $dot + (Get-Painted $full)
        }
        [void]$rows.Append('{"id":' + (ConvertTo-JsonText $id) + ',"content":' + (ConvertTo-JsonText $content) + "}`n")
    }
}

function Write-Stdout([string]$s) {
    $bytes = [Text.UTF8Encoding]::new($false).GetBytes($s)
    $stdout = [Console]::OpenStandardOutput()
    $stdout.Write($bytes, 0, $bytes.Length)
    $stdout.Flush()
}

$rows = [Text.StringBuilder]::new()
try {
    Split-Tokens (Read-Stdin)
    while ($script:p -lt $script:n) {
        Read-Value
        $in = $script:v
        try { Add-Rows $in $rows } catch { }
    }
} catch { }
try { Write-Stdout $rows.ToString() } catch { }
exit 0

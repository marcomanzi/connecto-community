# Claude Code status line on Windows: the PowerShell twin of statusline.sh
# (model, effort, context bar, used/max tokens, session price). It reads the
# same JSON on stdin and prints the same bytes, without bash, jq or awk, on
# Windows PowerShell 5.1 and PowerShell 7. Connecto runs it with
# `powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File`.
#
# How the original works, step by step, and what this file copies of it:
#  1. jq turns the input into one line of six tab-separated fields (a field
#     that is missing is "-"); a jq error or an input that is not JSON gives
#     an empty line. `a // b` takes b when a is null or false. An error on
#     its left (`.effort.level` of a string `effort`) is not caught: the
#     whole line is empty (jq 1.7, as macOS ships it). Only the model path
#     ends in `?` (`.model.display_name?`), which turns its error into "none".
#  2. `IFS=<tab> read` splits that line: runs of tabs count as one, so an
#     empty field moves the next ones left, and only the first line counts.
#  3. bash `printf '%.0f'`/`'%.2f'` and awk `%.1f`/`%d` format the numbers
#     (round half to even on the exact binary value, as C does).
# Never fails: whatever goes wrong prints what the original prints when jq
# gives nothing, and the exit code is 0.
#
# Keep this file pure ASCII (Windows PowerShell 5.1 reads a file without a
# BOM in the ANSI code page): non-ASCII output is built from code points.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$E = [string][char]27
$Dot = [string][char]0x00B7
$Full = [string][char]0x2588
$Light = [string][char]0x2591
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

# `.k1.k2...?`: like Get-Key, but an error gives null (jq's `?`).
function Get-Soft($value, [string[]]$keys) {
    foreach ($k in $keys) {
        if ($value -isnot $DictType) { return $null }
        $value = $value[[object]$k]
    }
    , $value
}

# `a // b`: b when a is null or false.
function Get-Or($a, $b) {
    if ($null -eq $a -or ($a -is [bool] -and -not $a)) { return , $b }
    , $a
}

# jq's `+`.
function Add-Values($a, $b) {
    if ($null -eq $a) { return , $b }
    if ($null -eq $b) { return , $a }
    if ($a -is [double] -and $b -is [double]) { return $a + $b }
    if ($a -is [string] -and $b -is [string]) { return $a + $b }
    throw 'cannot add'
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

# The jq program of statusline.sh, for one input value: the tab-joined line.
function Get-Fields($in) {
    $u = Get-Key (Get-Key $in 'context_window') 'current_usage'
    $model = Get-Or (Get-Soft $in 'model', 'display_name') 'Unknown model'
    $effort = Get-Or (Get-Key (Get-Key $in 'effort') 'level') '-'
    $used = Get-Or (Get-Key (Get-Key $in 'context_window') 'used_percentage') '-'
    if ($null -eq $u) {
        $tokens = '-'
    } else {
        $tokens = Add-Values (Add-Values (Get-Or (Get-Key $u 'input_tokens') 0.0) `
            (Get-Or (Get-Key $u 'cache_creation_input_tokens') 0.0)) `
            (Get-Or (Get-Key $u 'cache_read_input_tokens') 0.0)
    }
    $max = Get-Or (Get-Key (Get-Key $in 'context_window') 'context_window_size') '-'
    $cost = Get-Or (Get-Key (Get-Key $in 'cost') 'total_cost_usd') '-'
    $parts = foreach ($x in @($model, $effort, $used, $tokens, $max, $cost)) { ConvertTo-Text $x }
    $parts -join "`t"
}

# What jq prints first for the whole input (one line per input value; a value
# that fails prints nothing, and the first text that is not JSON ends it).
function Get-JqLine([string]$text) {
    Split-Tokens $text
    while ($script:p -lt $script:n) {
        Read-Value
        $in = $script:v
        try { return Get-Fields $in } catch { }
    }
    ''
}

# ---- bash and awk ------------------------------------------------------------

# `IFS=<tab> read -r a b c d e f`: the fields of the first line.
function Split-Line([string]$line) {
    $nl = $line.IndexOf("`n")
    if ($nl -ge 0) { $line = $line.Substring(0, $nl) }
    $rest = $line.TrimStart("`t")
    $out = [string[]]::new(6)
    for ($i = 0; $i -lt 5; $i++) {
        $tab = $rest.IndexOf("`t")
        if ($tab -lt 0) { $out[$i] = $rest; $rest = '' }
        else { $out[$i] = $rest.Substring(0, $tab); $rest = $rest.Substring($tab).TrimStart("`t") }
    }
    $out[5] = $rest.TrimEnd("`t")
    , $out
}

# The number at the start of a string, as strtod reads it (0 if none).
function Get-LeadingNumber([string]$s) {
    $m = [regex]::Match($s, '^[ \t\n\r\f\v]*([+-]?(?:[0-9]+\.?[0-9]*|\.[0-9]+)(?:[eE][+-]?[0-9]+)?)')
    if (-not $m.Success) { return 0.0 }
    ConvertTo-Number $m.Groups[1].Value
}

# C's `%.<digits>f` for 0 to 2 digits: the exact binary value, rounded half
# to even. The double is m * 2^e, so value * 10^digits is m * 5^digits *
# 2^(e + digits), and m * 5^digits < 2^58 fits a long: no big numbers needed
# unless the value is a whole number beyond 2^52 (then a decimal, or at worst
# a BigInteger, holds it).
function Format-Fixed([double]$d, [int]$digits) {
    if ([double]::IsNaN($d)) { return 'nan' }
    if ([double]::IsInfinity($d)) { if ($d -gt 0) { return 'inf' } else { return '-inf' } }
    $bits = [BitConverter]::DoubleToInt64Bits($d)
    $exp = [int](($bits -shr 52) -band 0x7FF)
    $man = $bits -band 0xFFFFFFFFFFFFF
    if ($exp -eq 0) { $exp = 1 } else { $man = $man -bor 0x10000000000000 }
    $a = $man * @(1L, 5L, 25L)[$digits]
    $shift = $exp - 1075 + $digits
    if ($shift -ge 0) {
        try {
            $q = [decimal]$a
            for ($i = 0; $i -lt $shift; $i++) { $q = $q * 2 }
            $s = $q.ToString($Inv)
        } catch {
            $s = [Numerics.BigInteger]::Multiply($a, [Numerics.BigInteger]::Pow(2, $shift)).ToString($Inv)
        }
    } else {
        $k = - $shift
        $q = 0L
        $up = $false
        if ($k -lt 62) {
            $q = $a -shr $k
            $r = $a - ($q -shl $k)
            $half = 1L -shl ($k - 1)
            $up = $r -gt $half -or ($r -eq $half -and ($q % 2) -eq 1)
        }
        if ($up) { $q++ }
        $s = $q.ToString($Inv)
    }
    if ($digits -gt 0) {
        $s = $s.PadLeft($digits + 1, [char]'0')
        $s = $s.Substring(0, $s.Length - $digits) + '.' + $s.Substring($s.Length - $digits)
    }
    if ($bits -lt 0) { $s = '-' + $s }
    $s
}

# A whole number in the range bash's `[` and `$((...))` work with.
function ConvertTo-Long([string]$s) {
    $x = 0L
    if ([long]::TryParse($s, [Globalization.NumberStyles]::AllowLeadingSign, $Inv, [ref]$x)) { return $x }
    0L
}

# human(): awk -v n="$1": 1.2M from a million on, else 123k.
function Format-Human([string]$s) {
    $v = Get-LeadingNumber $s
    if ($s -match '^[ \t\n]*[+-]?(?:[0-9]+\.?[0-9]*|\.[0-9]+)(?:[eE][+-]?[0-9]+)?[ \t\n]*$') {
        $big = $v -ge 1000000
    } else {
        $big = [string]::CompareOrdinal($s, '1000000') -ge 0
    }
    if ($big) {
        $t = Format-Fixed ($v / 1000000) 1
        if ($t.EndsWith('.0')) { $t = $t.Substring(0, $t.Length - 2) }
        return $t + 'M'
    }
    $k = [Math]::Truncate($v / 1000)
    if ([Math]::Abs($k) -lt 9e18) { return ([long]$k).ToString($Inv) + 'k' }
    (Format-Fixed $k 0) + 'k'
}

function Get-Repeat([string]$s, [long]$count) {
    if ($count -le 0) { return '' }
    $s * $count
}

# The printf part of statusline.sh.
function Format-Line([string]$line) {
    $f = Split-Line $line
    $model = $f[0]; $effort = $f[1]; $used = $f[2]; $tokens = $f[3]; $max = $f[4]; $cost = $f[5]
    $o = $E + '[36m' + $model + $E + '[0m'
    if ($effort -cne '-') { $o += ' ' + $E + '[90m' + $Dot + $E + '[0m ' + $E + '[35m' + $effort + $E + '[0m' }
    if ($used -cne '-') {
        $usedInt = Format-Fixed (Get-LeadingNumber $used) 0
        $n = ConvertTo-Long $usedInt
        $p = $n * 20
        $filled = [long](($p - ($p % 100)) / 100)
        if ($filled -gt 20) { $filled = 20 }
        if ($n -ge 50) { $color = $E + '[31m' }
        elseif ($n -ge 40) { $color = $E + '[38;5;214m' }
        else { $color = $E + '[33m' }
        $o += '  |  [' + $color + (Get-Repeat $Full $filled) + $E + '[90m' + (Get-Repeat $Light (20 - $filled)) +
        $E + '[0m] ' + $color + $usedInt + '%' + $E + '[0m'
    } else {
        $o += '  |  Context: ' + $E + '[90mno data' + $E + '[0m'
    }
    if ($tokens -cne '-' -and $max -cne '-') {
        $o += ' ' + $E + '[90m(' + (Format-Human $tokens) + '/' + (Format-Human $max) + ')' + $E + '[0m'
    } elseif ($max -cne '-') {
        $o += ' ' + $E + '[90m(max ' + (Format-Human $max) + ')' + $E + '[0m'
    }
    if ($cost -cne '-') { $o += '  |  ' + $E + '[32m$' + (Format-Fixed (Get-LeadingNumber $cost) 2) + $E + '[0m' }
    $o
}

function Write-Stdout([string]$s) {
    $bytes = [Text.UTF8Encoding]::new($false).GetBytes($s)
    $stdout = [Console]::OpenStandardOutput()
    $stdout.Write($bytes, 0, $bytes.Length)
    $stdout.Flush()
}

$text = $null
try { $text = Format-Line (Get-JqLine (Read-Stdin)) } catch { $text = $null }
if ($null -eq $text) { try { $text = Format-Line '' } catch { $text = '' } }
try { Write-Stdout $text } catch { }
exit 0

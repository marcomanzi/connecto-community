#!/bin/sh
# Claude Code subagent rows: name, the model that subagent runs on, effort,
# what it is doing, how long it has run, and its tokens. The main status line
# only knows the session's model, so this is where a subagent's own model shows.
# A row whose fields Claude Code hasn't sent yet keeps what it has.
#
# The input carries `columns` (the usable row width) and `tasks`. The elapsed
# time, tokens and percentage sit at the right edge of the row; the label gives
# way first when the row is narrow. Without a usable `columns` the row is laid
# out inline. Widths are counted in terminal columns from the plain text, never
# from the escape codes, so colours do not throw the alignment off.
jq -c '
  def esc(c): "\u001b[" + c + "m";
  def paint(c): esc(c) + . + esc("0");
  def dot: " " + ("·" | paint("90")) + " ";
  # Control characters (C0, DEL and C1) are dropped from every text field, so a
  # label cannot carry escape sequences or split the row over two lines.
  def clean: [explode[] | select(. >= 32 and . != 127 and (. < 128 or . > 159))] | implode;
  # claude-opus-5-5 -> Opus 5.5; other ids are shown as they are.
  def short:
    sub("\\[.*\\]$"; "") | sub("-[0-9]{8}$"; "")
    | if test("^claude-[a-z]+-[0-9]") then
        (sub("^claude-"; "") | split("-")) as $p
        | ($p[0][0:1] | ascii_upcase) + $p[0][1:] + " " + ($p[1:] | join("."))
      else . end;
  def human:
    if . >= 1000000 then ((. / 100000 | floor) / 10 | tostring) + "M"
    else ((. / 1000 | floor) | tostring) + "k" end;
  # Dollars and cents from a number of dollars.
  def money:
    (. * 100 | round) as $c
    | "$" + (($c / 100 | floor) | tostring) + "."
      + (($c % 100 | tostring) | if length < 2 then "0" + . else . end);
  def pad2: tostring | if length < 2 then "0" + . else . end;
  # Whole seconds to a compact span: 45s, 12m 34s, 1h 05m 09s.
  def elapsed:
    if . < 60 then (tostring + "s")
    elif . < 3600 then ((. / 60 | floor | tostring) + "m " + (. % 60 | pad2) + "s")
    else ((. / 3600 | floor | tostring) + "h " + ((. % 3600) / 60 | floor | pad2) + "m " + (. % 60 | pad2) + "s")
    end;
  # Terminal columns of one code point: combining marks take none, East Asian
  # wide and emoji take two, everything else one. Code points are decimal
  # because jq has no hex literals.
  def cpw:
    if . == 8203 or . == 8205 or (. >= 768 and . <= 879) or (. >= 65024 and . <= 65039) then 0
    elif (. >= 4352 and . <= 4447) or (. >= 11904 and . <= 42191) or (. >= 44032 and . <= 55203)
      or (. >= 63744 and . <= 64255) or (. >= 65072 and . <= 65103) or (. >= 65280 and . <= 65376)
      or (. >= 65504 and . <= 65510) or (. >= 127744 and . <= 129791)
      or (. >= 131072 and . <= 262141) then 2
    else 1 end;
  def cw: [explode[] | cpw] | add // 0;
  # The longest prefix that fits in $w columns, with an ellipsis when cut.
  def fit($w):
    if cw <= $w then .
    elif $w < 1 then ""
    else
      reduce explode[] as $c ({out: [], n: 0, stop: false};
        if .stop then .
        elif .n + ($c | cpw) <= $w - 1 then (.out += [$c] | .n += ($c | cpw))
        else .stop = true end)
      | ((.out | implode | sub(" +$"; "")) + "…")
    end;
  def plain($segs): $segs | map(.t) | add // "";
  def painted($segs):
    $segs | map(. as $s | if $s.c then ($s.t | paint($s.c)) else $s.t end) | add // "";
  # Padding is at most the clamped width (1000), so this stays small.
  def spaces($n): if $n > 0 then [range(0; $n) | " "] | add else "" end;
  def sep: {t: " · ", c: "90"};
  # Columns taken by a list of segments, and the gap kept before a right block.
  def width($s): plain($s) | cw;
  def gap($s): if ($s | length) > 0 then 2 else 0 end;
  # The head: name, then model and effort when the variant asks for them.
  def head($name; $model; $effort; $wm; $we):
    [{t: $name, c: "36"}]
    + (if $wm and $model != "" then [sep, {t: $model, c: "35"}] else [] end)
    + (if $we and $effort != "" then [sep, {t: $effort, c: "90"}] else [] end);
  # The first head that fits in $avail columns (full, then without effort, then
  # name only), or null when none does.
  def fitting_head($name; $model; $effort; $avail):
    [head($name; $model; $effort; true; true), head($name; $model; $effort; true; false),
     head($name; $model; $effort; false; false)]
    | map(select(width(.) <= $avail)) | .[0];
  # The left side in $avail columns: the head, then the label in what is left
  # after it (dropped when less than 8 columns remain); a name cut short when
  # even the bare head does not fit.
  def left($name; $model; $effort; $label; $avail):
    fitting_head($name; $model; $effort; $avail) as $h
    | if $h == null then [{t: ($name | fit($avail)), c: "36"}]
      else
        ($avail - width($h) - 3) as $room
        | if $label != "" and $room >= 8
          then $h + [sep, {t: ($label | fit($room)), c: null}]
          else $h end
      end;

  # A usable width is a whole number of at least 1, clamped to 1000. Anything
  # else (absent, string, fraction, zero, negative) means the width is unknown.
  (.columns) as $raw
  | (if ($raw | type) == "number" and $raw == ($raw | floor) and $raw >= 1
     then (if $raw > 1000 then 1000 else $raw end) else null end) as $W
  | ($W != null) as $known
  | .tasks[]?
  | (.id) as $id
  | (.name // .agentType // "agent" | clean) as $name
  | (.model // "" | clean | short) as $model
  | (.effort // "" | tostring | clean) as $effort
  | (.label // .description // "" | clean) as $label
  | ((.tokenCount // 0) | human) as $tokens
  | (if (.contextWindowSize // 0) > 0
     then " " + (((.tokenCount // 0) * 100 / .contextWindowSize | floor) | tostring) + "%"
     else "" end) as $used
  | (try .cost.total_cost_usd catch null) as $tc
  # Elapsed time since startTime (ms since the epoch), shown only for a task that
  # is running. A missing or null status counts as running: Claude Code may omit
  # the field, and a task with no status is not known to have finished. Any other
  # explicit value (including false or "completed") hides the time unless it is
  # the string "running". The start must be a positive whole number, not in the
  # future, and less than 30 days old, so a bogus startTime (0, negative, huge)
  # prints nothing rather than a bogus span.
  | (if (.startTime | type) == "number" and .startTime > 0 and .startTime == (.startTime | floor)
        and ((.status == null) or (.status == "running"))
     then ((now * 1000 - .startTime) / 1000 | floor) else null end) as $span
  | (if $span != null and $span >= 0 and $span < 2592000 then $span else null end) as $age
  | (if $age != null then [{t: ($age | elapsed), c: "90"}, {t: "  ", c: null}] else [] end) as $time
  | (if ($tc | type) == "number" then [sep, {t: ($tc | money), c: "33"}] else [] end) as $cost
  # The right block gives way in this order: the running time, then the token
  # count (the percentage is the last thing kept, and the cost goes with the tokens).
  | ([{t: ($tokens + $used), c: "33"}] + $cost) as $counts
  | ($time + $counts) as $full
  | (if $used != "" then [{t: ($used | sub("^ "; "")), c: "33"}] else [] end) as $pct
  | {id: $id, content: (
      if $known then
        # The first block that leaves room for the head beside it.
        ([$full, $counts, $pct]
          | map(select(fitting_head($name; $model; $effort; $W - width(.) - gap(.)) != null))
          | .[0]) as $tier
        | if $tier != null then
            ($W - width($tier) - gap($tier)) as $avail
            | left($name; $model; $effort; $label; $avail) as $l
            | painted($l) + spaces($W - width($l) - width($tier)) + painted($tier)
          else
            # Not even the bare name fits beside the block: keep the percentage,
            # cut the name, and cut the percentage too on a row narrower than it.
            (if width($pct) <= $W then $pct else [{t: (plain($pct) | fit($W)), c: "33"}] end) as $r
            | ($W - width($r) - gap($r)) as $avail
            | ($name | fit($avail)) as $n
            | painted([{t: $n, c: "36"}]) + spaces($W - ($n | cw) - width($r)) + painted($r)
          end
      else
        left($name; $model; $effort; $label; 120 - width($full) - 3) as $l
        | painted($l) + dot + painted($full)
      end
    )}'
exit 0

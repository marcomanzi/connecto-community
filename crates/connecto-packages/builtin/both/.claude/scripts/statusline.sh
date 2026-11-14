#!/bin/sh
# Claude Code status line: model, effort, context bar, used/max tokens, session price.
# Fields that Claude Code hasn't sent yet are left out.
input=$(cat)

IFS='	' read -r model effort used used_tokens max_tokens cost <<EOT
$(printf '%s' "$input" | jq -r '
  (.context_window.current_usage) as $u
  | [
      (.model.display_name? // "Unknown model"),
      (.effort.level // "-"),
      (.context_window.used_percentage // "-"),
      (if $u == null then "-"
       else ($u.input_tokens // 0) + ($u.cache_creation_input_tokens // 0)
            + ($u.cache_read_input_tokens // 0) end),
      (.context_window.context_window_size // "-"),
      (.cost.total_cost_usd // "-")
    ] | map(tostring) | join("\t")')
EOT

human() {
  awk -v n="$1" 'BEGIN {
    if (n >= 1000000) { s = sprintf("%.1f", n / 1000000); sub(/\.0$/, "", s); print s "M" }
    else printf "%dk\n", n / 1000
  }'
}

# Below 40% yellow, from 40% orange, from 50% red.
pick_color() {
  if [ "$1" -ge 50 ]; then printf '\033[31m'
  elif [ "$1" -ge 40 ]; then printf '\033[38;5;214m'
  else printf '\033[33m'
  fi
}

repeat() {
  i=0
  while [ "$i" -lt "$2" ]; do printf '%s' "$1"; i=$((i + 1)); done
}

printf '\033[36m%s\033[0m' "$model"
[ "$effort" != "-" ] && printf ' \033[90m·\033[0m \033[35m%s\033[0m' "$effort"

if [ "$used" != "-" ]; then
  used_int=$(printf '%.0f' "$used")
  filled=$((used_int * 20 / 100))
  [ "$filled" -gt 20 ] && filled=20
  color=$(pick_color "$used_int")
  printf '  |  [%s%s\033[90m%s\033[0m] %s%s%%\033[0m' \
    "$color" "$(repeat █ "$filled")" "$(repeat ░ $((20 - filled)))" "$color" "$used_int"
else
  printf '  |  Context: \033[90mno data\033[0m'
fi

if [ "$used_tokens" != "-" ] && [ "$max_tokens" != "-" ]; then
  printf ' \033[90m(%s/%s)\033[0m' "$(human "$used_tokens")" "$(human "$max_tokens")"
elif [ "$max_tokens" != "-" ]; then
  printf ' \033[90m(max %s)\033[0m' "$(human "$max_tokens")"
fi

[ "$cost" != "-" ] && printf '  |  \033[32m$%.2f\033[0m' "$cost"
exit 0

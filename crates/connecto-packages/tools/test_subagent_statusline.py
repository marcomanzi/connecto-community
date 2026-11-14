#!/usr/bin/env python3
"""Tests for builtin/both/.claude/scripts/subagent-statusline.sh.

Run: python3 crates/connecto-packages/tools/test_subagent_statusline.py

The script reads Claude Code's subagent JSON on stdin and writes one JSON line
per row, {"id", "content"}. The tests measure the visible width of `content`
(colour codes removed, wide characters counted as two columns) so the
right-hand block can be checked against the terminal width.
"""
import json
import os
import re
import subprocess
import time
import unicodedata
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, '..', 'builtin', 'both', '.claude', 'scripts', 'subagent-statusline.sh')
ANSI = re.compile(r'\x1b\[[0-9;]*m')


def visible_width(text):
    width = 0
    for ch in text:
        cp = ord(ch)
        if unicodedata.combining(ch) or cp in (0x200B, 0x200D) or 0xFE00 <= cp <= 0xFE0F:
            continue
        width += 2 if unicodedata.east_asian_width(ch) in ('W', 'F') else 1
    return width


def run(tasks, columns=None, timeout=30):
    payload = {'tasks': tasks}
    if columns is not None:
        payload['columns'] = columns
    out = subprocess.run(['sh', SCRIPT], input=json.dumps(payload).encode(),
                         stdout=subprocess.PIPE, check=True, timeout=timeout).stdout.decode()
    rows = [json.loads(line) for line in out.splitlines() if line.strip()]
    return rows


def plain_rows(tasks, columns=None):
    return [ANSI.sub('', r['content']) for r in run(tasks, columns)]


HAIKU = {
    'id': 't1', 'name': 'backend', 'model': 'claude-haiku-5-5', 'effort': 'medium',
    'label': 'Waiting for store and session tests', 'description': 'Waiting for store and session tests',
    'tokenCount': 172000, 'contextWindowSize': 1000000, 'status': 'running',
}


class SubagentStatusline(unittest.TestCase):
    def test_normal_width_right_aligns_tokens_and_percentage(self):
        for columns in (60, 120, 200):
            (row,) = plain_rows([HAIKU], columns)
            self.assertEqual(visible_width(row), columns, row)
            self.assertTrue(row.endswith('172k 17%'), row)
            self.assertIn('backend · Haiku 5.5 · medium · ', row)

    def test_the_colours_are_kept_and_the_right_block_is_yellow(self):
        (raw,) = [r['content'] for r in run([HAIKU], 120)]
        self.assertIn('\x1b[33m172k 17%\x1b[0m', raw)
        self.assertIn('\x1b[36mbackend\x1b[0m', raw)

    def test_a_long_label_is_cut_with_an_ellipsis_on_one_line(self):
        long = dict(HAIKU, label='Reviewing the whole connecto-packages engine and every install path for regressions in parallel')
        for columns in (60, 80):
            (row,) = plain_rows([long], columns)
            self.assertEqual(visible_width(row), columns, row)
            self.assertIn('…', row)
            self.assertTrue(row.endswith('172k 17%'), row)

    def test_a_narrow_row_drops_the_label_and_keeps_the_right_block(self):
        (row,) = plain_rows([HAIKU], 30)
        self.assertEqual(visible_width(row), 30, row)
        self.assertTrue(row.endswith('172k 17%'), row)
        self.assertNotIn('Waiting', row)

    def test_a_very_narrow_row_never_overflows(self):
        for columns in (10, 20):
            (row,) = plain_rows([HAIKU], columns)
            self.assertLessEqual(visible_width(row), columns, row)

    def test_non_ascii_description_is_measured_in_columns(self):
        task = dict(HAIKU, name='übersetzer', label='Übersetzung 日本語のテスト 🚀 done', tokenCount=8000,
                    contextWindowSize=200000)
        for columns in (60, 120):
            (row,) = plain_rows([task], columns)
            self.assertEqual(visible_width(row), columns, row)
            self.assertTrue(row.endswith('8k 4%'), row)

    def test_a_reported_cost_is_shown_after_the_tokens_in_dollars(self):
        task = dict(HAIKU, cost={'total_cost_usd': 1.2345})
        (row,) = plain_rows([task], 120)
        self.assertEqual(visible_width(row), 120, row)
        self.assertTrue(row.endswith('172k 17% · $1.23'), row)

    def test_a_missing_cost_shows_no_dollar_figure(self):
        (row,) = plain_rows([HAIKU], 120)
        self.assertNotIn('$', row)

    def test_a_cost_with_small_cents_keeps_two_digits(self):
        task = dict(HAIKU, cost={'total_cost_usd': 0.05})
        (row,) = plain_rows([task], 120)
        self.assertTrue(row.endswith('$0.05'), row)

    def test_without_a_width_the_row_is_inline(self):
        (row,) = plain_rows([HAIKU])
        self.assertIn('Waiting for store and session tests · 172k 17%', row)
        self.assertFalse(row.endswith(' '), row)

    def test_a_missing_context_window_shows_tokens_without_a_percentage(self):
        task = {'id': 't9', 'name': 'x', 'tokenCount': 500}
        (row,) = plain_rows([task], 80)
        self.assertEqual(visible_width(row), 80, row)
        self.assertTrue(row.endswith(' 0k'), row)
        self.assertNotIn('%', row)

    def test_one_line_per_row_with_ids(self):
        second = dict(HAIKU, id='t2', name='reviewer', model='claude-opus-5-5-20260101', effort='high')
        rows = run([HAIKU, second], 120)
        self.assertEqual([r['id'] for r in rows], ['t1', 't2'])
        self.assertIn('Opus 5.5', ANSI.sub('', rows[1]['content']))

    def test_no_tasks_prints_nothing(self):
        self.assertEqual(run([], 120), [])

    # Elapsed time: rendered from startTime (ms since the epoch) for a running task.

    def running_for(self, seconds, **extra):
        start = int(time.time() * 1000) - int(seconds * 1000)
        return dict(HAIKU, startTime=start, **extra)

    def assert_elapsed_near(self, row, expected, tolerance=2):
        m = re.search(r'(?:(\d+)h (\d\d)m (\d\d)s|(\d+)m (\d\d)s|(\d+)s)  172k', row)
        self.assertIsNotNone(m, row)
        if m.group(1):
            got = int(m.group(1)) * 3600 + int(m.group(2)) * 60 + int(m.group(3))
        elif m.group(4):
            got = int(m.group(4)) * 60 + int(m.group(5))
        else:
            got = int(m.group(6))
        self.assertLessEqual(abs(got - expected), tolerance, row)

    def test_elapsed_time_sits_left_of_the_tokens_at_the_right_edge(self):
        for columns in (60, 120, 200):
            (row,) = plain_rows([self.running_for(754)], columns)
            self.assertEqual(visible_width(row), columns, row)
            self.assertTrue(row.endswith('  172k 17%'), row)
            self.assert_elapsed_near(row, 754)

    def test_elapsed_time_is_dim_and_the_row_keeps_its_colours(self):
        (raw,) = [r['content'] for r in run([self.running_for(754)], 120)]
        self.assertRegex(raw, r'\x1b\[90m\d+m \d\ds\x1b\[0m  \x1b\[33m172k 17%\x1b\[0m$')

    def test_elapsed_time_formats(self):
        for seconds, shape in ((45, r'\d+s'), (754, r'\d+m \d\ds'), (3905, r'\d+h \d\dm \d\ds')):
            (row,) = plain_rows([self.running_for(seconds)], 120)
            m = re.search(r'(\d+h \d\dm \d\ds|\d+m \d\ds|\d+s)  172k', row)
            self.assertIsNotNone(m, row)
            self.assertRegex(m.group(1), '^' + shape + '$')

    def test_a_long_label_leaves_room_for_the_elapsed_time(self):
        long = self.running_for(754, label='Reviewing the whole connecto-packages engine and every install path for regressions')
        for columns in (60, 80):
            (row,) = plain_rows([long], columns)
            self.assertEqual(visible_width(row), columns, row)
            self.assertIn('…', row)
            self.assertRegex(row, r'\d+m \d\ds  172k 17%$')

    def test_without_a_width_the_elapsed_time_is_inline_before_the_tokens(self):
        (row,) = plain_rows([self.running_for(754)])
        self.assertRegex(row, r' · \d+m \d\ds  172k 17%$')

    def test_a_missing_start_time_leaves_no_gap(self):
        (row,) = plain_rows([HAIKU], 120)
        self.assertEqual(visible_width(row), 120, row)
        self.assertTrue(row.endswith('172k 17%'), row)
        self.assertNotRegex(row, r'\d+(h \d\dm|m \d\ds|s)\s+172k')

    def test_a_finished_task_shows_no_elapsed_time(self):
        (row,) = plain_rows([self.running_for(754, status='completed')], 120)
        self.assertTrue(row.endswith(' 172k 17%'), row)
        self.assertNotRegex(row, r'\d+(h \d\dm|m \d\ds|s)\s+172k')

    # Control characters from the input never reach the terminal.

    def test_control_characters_are_removed_and_the_row_stays_on_one_line(self):
        task = dict(HAIKU, label='x\x1b[31mred\nnew\tline\x7f', name='bad\x1bname', model='claude-haiku-5-5\r')
        rows = run([task], 120)
        self.assertEqual(len(rows), 1)
        raw = rows[0]['content']
        visible = ANSI.sub('', raw)
        self.assertFalse(any(ord(c) < 32 or ord(c) == 127 or 128 <= ord(c) <= 159 for c in visible), repr(visible))
        self.assertIn('x[31mred', visible)
        self.assertEqual(visible_width(visible), 120, visible)
        self.assertTrue(visible.endswith('172k 17%'), visible)

    def test_c1_control_characters_are_removed_from_the_label(self):
        task = dict(HAIKU, label='a\u009bb')
        (row,) = plain_rows([task], 120)
        self.assertIn('ab', row)
        self.assertNotIn('\u009b', row)

    # Width bounds: only a whole number from 1 up counts; huge values are clamped.

    def test_a_huge_width_is_clamped_and_returns_quickly(self):
        rows = run([HAIKU], 1e9, timeout=10)
        self.assertEqual(visible_width(ANSI.sub('', rows[0]['content'])), 1000)

    def test_unusable_widths_fall_back_to_the_inline_layout(self):
        for bad in (0, -5, 80.5, 'abc', '120', None, True):
            (row,) = plain_rows([HAIKU], bad)
            self.assertIn('Waiting for store and session tests · 172k 17%', row, repr(bad))
            self.assertFalse(row.endswith(' '), repr(bad))

    # Narrow rows: the right block gives way in order (label, running time, token
    # count with the percentage kept last), and no row is ever wider than columns.

    def test_a_row_narrower_than_the_right_block_never_overflows(self):
        for task in (HAIKU, self.running_for(754), dict(HAIKU, cost={'total_cost_usd': 12.5})):
            for columns in (1, 2, 3, 4, 5, 6, 8, 10, 12):
                (row,) = plain_rows([task], columns)
                self.assertLessEqual(visible_width(row), columns, (columns, row))
                self.assertGreater(visible_width(row), 0, (columns, row))

    def test_the_running_time_goes_before_the_token_count(self):
        task = self.running_for(754)
        (row,) = plain_rows([task], 20)
        self.assertEqual(visible_width(row), 20, row)
        self.assertTrue(row.endswith('172k 17%'), row)
        self.assertNotRegex(row, r'\d+m \d\ds')

    def test_the_token_count_goes_before_the_percentage(self):
        for columns in (12, 16):
            (row,) = plain_rows([self.running_for(754)], columns)
            self.assertEqual(visible_width(row), columns, row)
            self.assertTrue(row.endswith('17%'), row)
            self.assertNotIn('172k', row)
            self.assertIn('backend', row)

    def test_the_cost_goes_with_the_token_count(self):
        (row,) = plain_rows([dict(HAIKU, cost={'total_cost_usd': 1.5})], 16)
        self.assertTrue(row.endswith('17%'), row)
        self.assertNotIn('$', row)
        self.assertLessEqual(visible_width(row), 16)

    def test_the_label_goes_first_on_a_row_that_still_fits_the_time(self):
        (row,) = plain_rows([self.running_for(754, label='Reviewing every install path')], 60)
        self.assertIn('…', row)
        self.assertRegex(row, r'\d+m \d\ds  172k 17%$')

    def test_a_row_of_two_columns_shows_part_of_the_percentage(self):
        (row,) = plain_rows([HAIKU], 2)
        self.assertLessEqual(visible_width(row), 2, row)
        self.assertNotEqual(row.strip(), '', row)

    # Start times: only a positive whole number that is not in the future and is
    # under 30 days old shows an elapsed time; anything else prints nothing.

    def assert_no_elapsed(self, task):
        (row,) = plain_rows([task], 120)
        self.assertEqual(visible_width(row), 120, row)
        self.assertTrue(row.endswith('172k 17%'), row)
        self.assertNotRegex(row, r'\d+(h \d\dm|m \d\ds|s)  172k')
        self.assertNotIn('e+', row)
        return row

    def test_a_zero_start_time_shows_no_elapsed_time(self):
        self.assert_no_elapsed(dict(HAIKU, startTime=0))

    def test_a_negative_or_huge_start_time_shows_no_elapsed_time(self):
        self.assert_no_elapsed(dict(HAIKU, startTime=-1e300))
        self.assert_no_elapsed(dict(HAIKU, startTime=1e300))
        self.assert_no_elapsed(dict(HAIKU, startTime=-5))

    def test_a_start_time_in_the_future_shows_no_elapsed_time(self):
        future = int(time.time() * 1000) + 3600 * 1000
        self.assert_no_elapsed(dict(HAIKU, startTime=future))

    def test_a_start_time_older_than_thirty_days_shows_no_elapsed_time(self):
        old = int(time.time() * 1000) - 31 * 86400 * 1000
        self.assert_no_elapsed(dict(HAIKU, startTime=old))

    def test_a_fractional_start_time_shows_no_elapsed_time(self):
        start = int(time.time() * 1000) - 754000 + 0.5
        self.assert_no_elapsed(dict(HAIKU, startTime=start))

    def test_a_string_start_time_shows_no_elapsed_time(self):
        self.assert_no_elapsed(dict(HAIKU, startTime=str(int(time.time() * 1000))))

    def test_a_start_time_of_twenty_nine_days_still_shows_hours(self):
        (row,) = plain_rows([self.running_for(29 * 86400)], 120)
        self.assertRegex(row, r'\d+h \d\dm \d\ds  172k 17%$')

    def test_an_hour_shows_hours_minutes_and_seconds(self):
        (row,) = plain_rows([self.running_for(3905)], 120)
        self.assertRegex(row, r'1h 05m \d\ds  172k 17%$')
        self.assert_elapsed_near(row, 3905)

    def test_seconds_are_zero_padded_under_an_hour_too(self):
        (row,) = plain_rows([self.running_for(3660 + 7)], 120)
        self.assertRegex(row, r'1h 01m 0\ds  172k')

    # Status: a missing or null status counts as running (Claude Code may omit
    # it), and only an explicit non-running status hides the time.

    def test_a_missing_status_shows_the_elapsed_time(self):
        task = self.running_for(754)
        del task['status']
        (row,) = plain_rows([task], 120)
        self.assert_elapsed_near(row, 754)

    def test_a_null_status_shows_the_elapsed_time(self):
        (row,) = plain_rows([self.running_for(754, status=None)], 120)
        self.assert_elapsed_near(row, 754)

    def test_an_explicit_running_status_shows_the_elapsed_time(self):
        (row,) = plain_rows([self.running_for(754, status='running')], 120)
        self.assert_elapsed_near(row, 754)

    def test_an_explicit_finished_status_hides_the_elapsed_time(self):
        for status in ('completed', 'done', 'failed', False):
            (row,) = plain_rows([self.running_for(754, status=status)], 120)
            self.assertNotRegex(row, r'\d+(h \d\dm|m \d\ds|s)  172k', (status, row))
            self.assertTrue(row.endswith('172k 17%'), (status, row))


if __name__ == '__main__':
    unittest.main()

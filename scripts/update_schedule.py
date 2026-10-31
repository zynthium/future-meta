#!/usr/bin/env python3
"""Offline admission for official updates; never import or contact a collector."""
import argparse
import json
from datetime import datetime, time, timedelta
from pathlib import Path
from zoneinfo import ZoneInfo

CALENDAR = Path(__file__).resolve().parents[1] / 'config/update-calendar.json'
CHINA = ZoneInfo('Asia/Shanghai')


def weekday_neighbor(day, direction):
    day += timedelta(days=direction)
    while day.weekday() >= 5:
        day += timedelta(days=direction)
    return day


def decide(now=None):
    now = now or datetime.now(CHINA)
    if now.tzinfo is None:
        raise ValueError('Schedule timestamp requires a timezone')
    now = now.astimezone(CHINA)
    day = now.date()
    result = {'should_update': False, 'slot': None, 'evaluated_at': now.isoformat()}

    def skip(reason):
        return {**result, 'reason': reason}

    if day.weekday() >= 5:
        return skip('weekend')
    evening = time(18, 30) <= now.time() < time(20)
    morning = time(8) <= now.time() < time(8, 20)
    if not evening and not morning:
        return skip('outside_access_window')

    calendar = json.loads(CALENDAR.read_text())
    if calendar['schema'] != 1:
        raise ValueError('Unsupported schedule calendar schema')

    def trading_day(candidate):
        year = calendar['years'].get(str(candidate.year))
        if year is None:
            raise ValueError(f'Official schedule calendar missing for {candidate.year}; update it before collecting')
        return candidate.isoformat() not in year['closed_weekdays']

    if not trading_day(day):
        return skip('exchange_holiday')
    if evening:
        # Friday's night session belongs to Monday. A statutory weekday
        # closure instead means there is no holiday-eve night session.
        if not trading_day(weekday_neighbor(day, 1)):
            return skip('no_night_before_holiday')
        slot = 'evening'
    else:
        if trading_day(weekday_neighbor(day, -1)):
            return skip('ordinary_morning')
        slot = 'holiday_morning'
    return {**result, 'should_update': True, 'slot': slot, 'reason': 'session_due'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--at', type=datetime.fromisoformat, help='Offline preview timestamp with timezone')
    parser.add_argument('--github-output', type=Path)
    args = parser.parse_args()
    result = decide(args.at)
    if args.github_output:
        with args.github_output.open('a') as output:
            output.write(f"should_update={str(result['should_update']).lower()}\n")
    print(json.dumps(result))


if __name__ == '__main__':
    main()

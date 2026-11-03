#!/usr/bin/env python3
"""Offline admission for official updates; never import or contact a collector."""
import argparse
import json
from datetime import datetime, time, timedelta
from pathlib import Path
from zoneinfo import ZoneInfo

CALENDAR = Path(__file__).resolve().parents[1] / 'config/update-calendar.json'
CHINA = ZoneInfo('Asia/Shanghai')


def access_deadline(gate):
    if not gate['should_update']:
        raise ValueError('No admitted official access window')
    evaluated = datetime.fromisoformat(gate['evaluated_at']).astimezone(CHINA)
    return evaluated.replace(
        hour=20 if gate['slot'] == 'evening' else 8,
        minute=0 if gate['slot'] == 'evening' else 20,
        second=0, microsecond=0,
    ).isoformat()


def weekday_neighbor(day, direction):
    day += timedelta(days=direction)
    while day.weekday() >= 5:
        day += timedelta(days=direction)
    return day


def china_time(now=None):
    now = now or datetime.now(CHINA)
    if now.tzinfo is None:
        raise ValueError('Schedule timestamp requires a timezone')
    return now.astimezone(CHINA)


def load_calendar():
    calendar = json.loads(CALENDAR.read_text())
    if calendar['schema'] != 1:
        raise ValueError('Unsupported schedule calendar schema')
    return calendar


def trading_day(day, calendar):
    year = calendar['years'].get(str(day.year))
    if year is None:
        raise ValueError(f'Official schedule calendar missing for {day.year}; update it before collecting')
    return day.weekday() < 5 and day.isoformat() not in year['closed_weekdays']


def decide(now=None, calendar=None):
    now = china_time(now)
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

    calendar = load_calendar() if calendar is None else calendar
    if not trading_day(day, calendar):
        return skip('exchange_holiday')
    if evening:
        # Friday's night session belongs to Monday. A statutory weekday
        # closure instead means there is no holiday-eve night session.
        if not trading_day(weekday_neighbor(day, 1), calendar):
            return skip('no_night_before_holiday')
        slot = 'evening'
    else:
        if trading_day(weekday_neighbor(day, -1), calendar):
            return skip('ordinary_morning')
        slot = 'holiday_morning'
    return {**result, 'should_update': True, 'slot': slot, 'reason': 'session_due'}


def collection_target(now, calendar=None):
    """Last completed trading date; holiday mornings collect the pre-break day."""
    now = china_time(now)
    calendar = load_calendar() if calendar is None else calendar
    day = now.date() if now.time() >= time(18, 30) else now.date() - timedelta(days=1)
    for _ in range(370):
        if trading_day(day, calendar):
            return day.isoformat()
        day -= timedelta(days=1)
    raise ValueError('No completed trading day in approved calendar')


def latest_completed_slot(now=None):
    """Expected checkpoint after a slot closes, allowing ten minutes to publish.

    Reuse collection admission, so weekends, holiday eves and ordinary mornings
    cannot invent a new collection obligation. No network access is involved.
    """
    now = china_time(now)
    calendar = load_calendar()
    for offset in range(370):
        day = now.date() - timedelta(days=offset)
        for hour, minute in ((18, 30), (8, 0)):
            candidate = datetime.combine(day, time(hour, minute), CHINA)
            # Avoid evaluating future windows (including unknown future years).
            end = candidate.replace(hour=20, minute=0) if hour == 18 else candidate.replace(minute=20)
            if end + timedelta(minutes=10) > now:
                continue
            gate = decide(candidate, calendar)
            if gate['should_update']:
                return {
                    'slot': f"{day.isoformat()}:{gate['slot']}",
                    'target_through': collection_target(candidate, calendar),
                }
    raise ValueError('No completed update slot in approved calendar')


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

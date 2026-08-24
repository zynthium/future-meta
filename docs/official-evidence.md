# Official historical-fee evidence

Historical fee changes must be staged in `data/official-evidence.sqlite` before
anyone considers adding them to the production history. This database is
separate from `data/future-meta.sqlite`, is never exported to Pages, and cannot
change daily `update-latest` behaviour.

Only original exchange URLs are accepted as `canonical_url`. HTTPS is required
except for the CFFEX, DCE and GFEX primary domains, whose historical notices and
parameter archives may be served only over HTTP. Keep that `http://` URL
unchanged; do not invent an HTTPS equivalent. HTTP is rejected for every other
exchange.
An optional `mirror_url` is only for retrieval convenience. The SHA-256 must
be calculated from the exact bytes retrieved from `canonical_url`; do not copy
a digest from another page or a third-party repost.

CFFEX/DCE/GFEX HTTP evidence has no transport-level authenticity guarantee. Retain
the exact downloaded bytes and require human review of their content, URL, and
digest before staging or promotion. This exception only permits isolated
evidence staging; it does not permit production promotion.

Each staged adjustment needs at least one fee field. A `verified` adjustment
also needs both of the following first-party documents:

- an effective-date announcement (`notice`);
- a fee schedule or an effective-day parameter file (`fee_schedule` or
  `settlement_parameter`).

`verified` means that this required document pair is present; the daemon does
not OCR or infer the documents' contents. Keep the original downloaded bytes
under a local, untracked evidence directory named by SHA-256, and have a human
review the stated fee, unit, effective time, and contract list before any
promotion review.

The documents can prove only the fields they state. Omit every unchanged or
unstated field rather than inferring it. A staged adjustment is not a complete
fee rule and cannot be exported.

## Evidence levels

Two evidence levels may materialize a complete fee tuple in a review copy:

- `paired_official`: an effective-date notice paired with a fee schedule or
  parameter file;
- `official_parameter`: a retained first-party parameter table that directly
  states the complete open, close-yesterday and close-today tuple with explicit
  units.

`official_parameter` is intentionally lower confidence. It is allowed when no
paired announcement has been retained, but only after the importer verifies the
exchange URL, exact retained-byte SHA-256, complete tuple and units. It cannot
overwrite a `paired_official` version. Identical adjacent observations are
coalesced and conflicting third-party rows inside the retained observation
interval are removed atomically.

The CZCE importer maps `交易手续费` to open and close-yesterday, and
`平今仓手续费` or `日内平今仓交易手续费` to close-today. `绝对值` means yuan per
lot; `比例值` means per ten-thousand of turnover. Legacy pages without the mode
column explicitly document yuan per lot in their notes. These fee pages do not
state contract multipliers or tick sizes, so they cannot promote specification
history to official provenance.

The GFEX importer maps `openFee`, `offsetFee`, and `shortOffsetFee` to open,
close-yesterday, and close-today respectively. `绝对值` means yuan per lot,
`比例值` means per ten-thousand of turnover, and an explicit numeric zero is
retained as `FeeKind::Zero`. Only `ok` manifest rows are evidence. An
`official_empty` or `pending` row remains a coverage gap and cannot support a
completeness claim.

## Materialize paired history

Use `import-official-history` only against a review copy. It reads one or more
reviewed JSON inputs, a staged evidence database, or both. An optional exchange
filter limits a run to one exchange.

```bash
cargo run -p future-meta-daemon -- import-official-history \
  --db path/to/review-copy.sqlite \
  --evidence-db data/official-evidence.sqlite \
  --exchange CFFEX \
  --snapshot-dir data/official-evidence \
  --from 2020-01-01 \
  --through 2026-08-24
```

Every referenced digest must resolve to exactly one retained file and match
its bytes. The importer rejects unverified records and incomplete document
pairs. A partial adjustment may inherit unstated fields only from an earlier
complete tuple for the same concrete contract. It never inherits from another
contract, a third-party baseline, or a later observation.

GFEX complete daily settlement parameters can instead be imported directly
into a review copy at the lower `official_parameter` evidence level:

```bash
cargo run -p future-meta-daemon -- import-gfex-parameters \
  --db path/to/review-copy.sqlite \
  --manifest data/official-evidence/gfex-daily-settlement-20221222-20260818.tsv \
  --snapshot-dir data/official-evidence \
  --from 2022-12-22
```

The command validates every selected official URL and retained-byte digest
before atomically replacing conflicting lower-confidence history. It never
overwrites a `paired_official` interval.

INE `TRADEFEERATIO` and `TRADEFEEUNIT` fields state only the general fee. They
cannot establish close-today fees by themselves. Pair the retained dailydata
manifest with a reviewed close-today rule manifest:

```bash
cargo run -p future-meta-daemon -- import-ine-parameters \
  --db path/to/review-copy.sqlite \
  --manifest data/official-evidence/ine-dailydata-20200101-20260818.tsv \
  --close-today-rules data/official-evidence/ine-close-today-rules.tsv \
  --snapshot-dir data/official-evidence \
  --from 2020-01-01
```

The rule TSV columns are `scope`, `valid_from`, `valid_to`,
`close_today_kind`, `close_today_value`, `canonical_url`, and `sha256`.
`scope` is either an INE product or concrete contract. Allowed rule kinds are
`same_as_general`, `Zero`, `CnyPerLot`, and
`TurnoverRatePerTenThousand`. Every daily parameter observation must match one
unambiguous highest-priority rule. Missing or same-scope overlapping rules,
unretained bytes, invalid URLs, and digest mismatches fail before database
writes. Materialized versions retain
both sources at `paired_official` level.

## Materialize lifecycle and specifications

Lifecycle and specification evidence is independent from fee evidence. Import
it from a reviewed TSV into the same review copy:

```bash
cargo run -p future-meta-daemon -- import-official-metadata \
  --db path/to/review-copy.sqlite \
  --manifest data/official-evidence/official-metadata.tsv \
  --snapshot-dir data/official-evidence
```

Each row states a concrete symbol, listing and expiry dates, one contiguous
specification interval, lot size, tick size, and separate URL/digest pairs for
lifecycle and specification evidence. All intervals for a contract must cover
its entire listed lifetime without gaps. The strict coverage audit also
requires retained evidence links for every official fee, specification, and
lifecycle claim.

### SHFE and INE contract-base snapshots

SHFE and INE publish exact listing and expiry dates in dated
`ContractBaseInfoYYYYMMDD.dat` snapshots. Import a retained monthly corpus into
a review copy:

```bash
cargo run -p future-meta-daemon -- import-contract-base-info \
  --db path/to/review-copy.sqlite \
  --exchange SHFE \
  --manifest data/official-evidence/shfe-contract-base-info.tsv \
  --snapshot-dir data/official-evidence
```

The manifest columns are `exchange`, `report_date`, `canonical_url`, `sha256`,
and `record_count`. Only the exchange's exact official
`/data/busiparamdata/future/ContractBaseInfoYYYYMMDD.dat` URL is accepted. Every
database contract for the selected exchange must occur in at least one retained
snapshot; missing coverage fails before writes.

Exchange files may use space-padded contract identifiers. SHFE files can also
contain INE rows, so the importer limits observations to concrete contracts
already present for the selected exchange. A later official snapshot can revise
a not-yet-expired contract's expiry date after a holiday-calendar adjustment.
Only snapshots stating the final observed boundary are linked as lifecycle
evidence; listing-date conflicts remain fatal.

### CFFEX product and calendar evidence

CFFEX publishes exact contract last-trading-day events in its monthly trading
calendar XML. Import reviewed product specification history together with those
retained calendar snapshots:

```bash
cargo run -p future-meta-daemon -- import-cffex-metadata \
  --db path/to/review-copy.sqlite \
  --product-manifest data/official-evidence/cffex-product-metadata.tsv \
  --calendar-manifest data/official-evidence/cffex-calendar-2019-2026.tsv \
  --snapshot-dir data/official-evidence
```

The product TSV columns are `product`, `valid_from`, `valid_to`, `lot_size`,
`tick_size`, `expiry_rule`, `specification_url`, and
`specification_sha256`. `expiry_rule` is `second_friday` for government-bond
futures and `third_friday` for stock-index futures. Product intervals must be
contiguous. TS retains its initial contract and the official 2023-11-06
tick-size adjustment as separate intervals.

The calendar TSV columns are `month`, `canonical_url`, and `sha256`. Calendar
URLs must use the exact CFFEX `/sj/jyrl/YYYYMM/index_6782.xml` path. Each covered
contract month must contain an explicit `最后交易日` event; a missing event fails
the import. The importer uses the official product rule only outside retained
calendar months, for pre-calendar expired contracts and distant listed
contracts whose expiry calendar has not yet been published.

Each contract retains separate lifecycle evidence for listing and expiry.
Listing evidence comes from the already paired official CFFEX listing notice;
expiry evidence comes from exact calendar XML or, outside calendar coverage,
the official product contract rule.

## Stage a reviewed document pair

Download the official source files through an approved human-accessible path,
preserve the original bytes locally, and calculate their digests. Create a JSON
array with one object per concrete TqSdk futures symbol:

```json
[
  {
  "symbol": "INE.sc2604",
  "effective_at": "2026-03-10T00:00:00+08:00",
  "scope": "all listed SC contracts",
  "open_fee": null,
  "close_yesterday_fee": null,
  "close_today_fee": {
    "kind": "CnyPerLot",
    "value": 60.0,
    "raw_text": "60元/手"
  },
  "evidence": [
    {
      "canonical_url": "https://www.ine.cn/eng/circularnews/circular/202603/t20260306_830603.html",
      "mirror_url": "https://www.ine.cn/publicnotice/notice/202603/t20260306_830600.html",
      "sha256": "<sha256 of the English official notice bytes>",
      "published_at": "2026-03-06T00:00:00+08:00",
      "kind": "notice"
    },
    {
      "canonical_url": "https://www.ine.cn/publicnotice/notice/202603/W020260306643830614686.doc",
      "mirror_url": null,
      "sha256": "<sha256 of the official attachment bytes>",
      "published_at": "2026-03-06T00:00:00+08:00",
      "kind": "fee_schedule"
    }
  ]
  }
]
```

Then stage it without any network request:

```bash
cargo run -p future-meta-daemon -- stage-official \
  --db data/official-evidence.sqlite \
  --input path/to/adjustment.json
```

Allowed fee kinds are `CnyPerLot`, `TurnoverRatePerTenThousand`, and `Zero`.
`Unknown` values, malformed TqSdk symbols, non-exchange canonical domains,
HTTP URLs outside the CFFEX exception, invalid timestamps, and invalid SHA-256
values are rejected.

## CFFEX 2020 candidate boundary

`data/official-evidence/cffex-2020-listing-candidates.json` stages 48 verified
CFFEX listing-day candidates, backed by 22 retained official documents. They
cover all 2020 newly listed futures contracts:

- IF, IH, IC: 12 contracts each, `0.23/万分之` open/close-yesterday and
  `3.45/万分之` close-today;
- T, TF, TS: 4 contracts each, `3元/手` open/close-yesterday and zero
  close-today.

The 2019-12-23 through 2020-12-30 parameter-file sequence contains no fee
change for any of the 69 CFFEX futures contracts observed during 2020. The
other 21 were already listed before 2020; they are intentionally not recorded
as fictional 2020 adjustments.

`data/official-evidence/cffex-2023-03-20-close-today.json` stages 12 more
verified records for IF/IH/IC/IM 2304, 2306, and 2309. The 2023-03-17 CFFEX
notice and the 2023-03-20 parameter file agree that close-today changed from
`3.45/万分之` to `2.3/万分之`; open and close-yesterday are deliberately omitted
because the notice did not change them.

`data/official-evidence/cffex-2021-2023-listing-candidates.json` stages 171
more verified listing-day records from 2021-01-18 through 2023-12-18:

- IF, IH and IC: 36 contracts each;
- IM: 21 contracts, including its first four contracts on 2022-07-22;
- T, TF and TS: 12 contracts each;
- TL: 6 contracts, including its first three contracts on 2023-04-21.

The companion `cffex-2021-2023-listing-audit.json` retains the first-observed
contract check across all 36 official monthly history packages and references
the 50 same-day settlement-parameter files used to read the three fee fields.
Every cited raw ZIP, HTML and CSV byte sequence is retained by SHA-256 under
the untracked evidence directory. The 2023-03-20 equity-index close-today
change is applied only on or after that effective day; no later fee is
projected backwards.

`data/official-evidence/cffex-2024-2026-listing-candidates.json` stages a
further 164 verified listing-day records from 2024-01-22 through 2026-07-20:

- IF, IH, IC and IM: 31 contracts each;
- T, TF, TS and TL: 10 contracts each.

Its audit file retains 31 official monthly history packages and 41 listing-day
parameter files. Together, the 2021–2026 batches add 335 contract listings;
with the previous 48 2020 listings and 12 March 2023 changes, the isolated
CFFEX evidence store contains 395 verified adjustments. This count is an
evidence-coverage measure, not permission to export a reconstructed history.

`data/official-evidence/cffex-2019-q1-listing-candidates.json` adds six
verified 2019 listing-day records, each with retained original listing notice
and effective-day settlement-parameter bytes:

- T1912, TF1912 and TS1912 on 2019-03-11: `3元/手`, `3元/手`, and zero
  close-today fee;
- IF1912, IH1912 and IC1912 on 2019-04-22: `万分之0.23`, `万分之0.23`, and
  `万分之3.45` close-today fee.

The CFFEX isolated total is therefore 401 verified adjustments. The raw
2019-03-08 and 2019-04-19 listing notices, and their two same-day parameter
files, are retained by SHA-256 under `data/official-evidence/`.

`data/official-evidence/cffex-2019-q2-listing-candidates.json` adds six more
verified listing-day records: T2003, TF2003 and TS2003 on 2019-06-17, then
IF1908, IH1908 and IC1908 on 2019-06-24. Their two original listing notices
and two effective-day parameter files are retained by SHA-256. The CFFEX
isolated total is now 407 verified adjustments.

The 2019 monthly archive audit identified 48 futures contracts first observed
after the December 2018 baseline. All 48 are now staged as verified listing-day
records, with an official listing notice and effective-day settlement parameter
file for each listing group. The CFFEX isolated total is now 443 verified
adjustments. This remains isolated evidence only; it does not establish a
continuous production history for contracts listed before 2019.

The 2018 audit is complete for the 48 futures contracts first observed after
the December 2017 baseline. All 48 are staged as verified listing-day records
across 17 listing dates from 2018-01-22 through 2018-12-24. Every record has
all three fee fields plus exactly one retained CFFEX listing notice and one
effective-day settlement-parameter file (34 distinct original documents).

The equity-index parameter files before the 2018-12-02 fee adjustment state
`万分之0.23` and a `3000%` close-today rate; this is preserved as
`万分之0.23 × 3000%` (万分之6.9). The 2018-12-24 parameter file states `2000%`,
preserved as 万分之4.6. Treasury-futures open and close-yesterday fees are
`3元/手`, with explicit zero close-today fee. The CFFEX isolated total was 491
verified adjustments at this boundary.

The 2017 audit is complete against the December 2016 archive baseline. The
archive scan identifies 44 (not 48) first-observed contracts, and all 44 are
verified across 16 listing dates from 2017-01-23 through 2017-12-18. Every
record has all three fee fields plus exactly one retained listing notice and
one effective-day parameter file (32 distinct original documents). The
2017-01-23 table states
`万分之0.23` and `10000%` for close-today, preserved as
`万分之0.23 × 10000%` (万分之23); subsequent verified index listings state
`4000%`, preserved as 万分之9.2. The 2017-03-13 T and TF contracts are kept
separate: T has explicit zero close-today fee, while TF has `100%` (3元/手).
The CFFEX isolated total is now 535 verified adjustments.

The 2016 audit is complete against the December 2015 baseline: all 44
first-observed contracts are verified across 16 listing dates from 2016-01-18
through 2016-12-19. Every record has all three fee fields plus exactly one
retained listing notice and one effective-day parameter file (32 distinct
original documents). The index parameter files state
`万分之0.23` and `10000%` close-today rate (万分之23). T has explicit zero
close-today fee; TF has `100%` (3元/手). Each staged record retains an official
notice and effective-day parameter file. The CFFEX isolated total is now 579
verified adjustments.

The 2015 audit is complete against the December 2014 baseline: all 45
first-observed contracts are verified across 17 listing dates from 2015-01-19
through 2015-12-21. Every record has all three fee fields plus exactly one
retained listing notice and one effective-day parameter file (36 distinct
original documents), including the first listed IH and IC contracts on
2015-04-16. Their initial parameter table gives all three fees as `万分之0.25`.
The TF1512 listing is backed by its dedicated 2015-03-13 notice (not the
separate 10-year Treasury notice), and its parameter file gives explicit zero
close-today fee. The CFFEX isolated total is now 624 verified adjustments.

The 2014 audit is complete against the December 2013 baseline: all 16
first-observed contracts are verified across 16 listing dates from 2014-01-20
through 2014-12-22. Every record has all three fee fields plus exactly one
retained listing notice and one effective-day parameter file (32 distinct
original documents). IF contracts use `万分之0.25` for all three fields; TF
contracts use `3元/手` open/close-yesterday and explicit zero close-today.
The CFFEX isolated total is now 640 verified adjustments.

The 2013 audit is complete against the December 2012 baseline: all 16
first-observed contracts are verified across 14 listing dates from 2013-01-21
through 2013-12-23. Every record has all three fee fields plus exactly one
retained listing notice and one effective-day parameter file (28 distinct
original documents). The inaugural TF parameter file leaves its base fee blank,
but its listing notice explicitly states `3元/手` and the parameter file states
`100%` close-today; this is recorded as 3元/手 for all three fields. The CFFEX
isolated total is now 656 verified adjustments.

The 2012 audit is complete against the December 2011 archive baseline: all 12
first-observed IF contracts are verified across 12 listing dates from 2012-01-30
through 2012-12-24. Every record has all three fee fields plus exactly one
retained listing notice and one effective-day parameter file (24 distinct
original documents). The parameter files show actual listing-day rates:
`万分之0.32` through May, `万分之0.35` from June through August, and
`万分之0.25` from September; every listed record has `100%` close-today rate.
The CFFEX isolated total is now 668 verified adjustments.

The 2011 audit is complete against the December 2010 parameter baseline: all 12
first-observed IF contracts are verified across 12 listing dates from 2011-01-24
through 2011-12-19. Every record has all three fee fields plus exactly one
retained listing notice and one effective-day parameter file (24 distinct
original documents). Every listed contract has `万分之0.5` open and
close-yesterday fee, with an explicit `100%` close-today rate. The CFFEX
isolated total is now 680 verified adjustments.

The 2010 post-launch audit is complete for the eight new IF contracts observed
after the first listing day, from 2010-05-24 through 2010-12-20. Each has all
three fee fields plus exactly one retained listing notice and one effective-day
parameter file (16 distinct original documents); each shows `万分之0.5` and
an explicit `100%` close-today rate. The CFFEX isolated total is now 688
verified adjustments. The initial four contracts (`IF1005`, `IF1006`,
`IF1009`, `IF1012`) remain intentionally unstaged: their 2010-03-26 first-party
listing notice states `万分之0.5`, but the public archive has no 2010-04-16
effective-day parameter file. The retained 2010-04-14 table is not substituted
for that missing effective-day evidence.

The 2012 fee-change audit adds eight non-listing adjustments. The 2012-04-27
notice and 2012-06-01 parameter file verify that `IF1206`, `IF1207`, `IF1209`,
and `IF1212` changed to `万分之0.35`; the 2012-08-02 notice and 2012-09-03
parameter file verify that `IF1209`, `IF1210`, `IF1212`, and `IF1303` changed
to `万分之0.25`. Both effective-day files explicitly state `100%` close-today
rate, so all three fee fields are present for every record. The CFFEX isolated
total is now 696 verified adjustments.

The 2015 fee-change audit adds 39 verified adjustments: on 2015-08-03, all
12 listed IF/IH/IC contracts changed to `万分之0.23`; on 2015-08-26 and
2015-09-07, the 12 then-listed IF/IH/IC contracts' close-today fee changed to
`万分之1.15` and `万分之23`, respectively; and on 2015-12-01, the three
listed TF contracts' close-today fee changed to `3元/手`. For the latter three
events, the unchanged open and close-yesterday fields are deliberately absent
from the adjustment record; their effective-day parameter files are retained
and were reviewed, but the notices only adjust close-today fee. The CFFEX
isolated total is now 735 verified adjustments.

The 2017-2018 fee-change audit adds 27 verified close-today adjustments: 12
IF/IH/IC contracts at `万分之9.2` on 2017-02-17, 12 IF/IH/IC contracts at
`万分之6.9` on 2017-09-18, and three TF contracts at explicit zero on
2018-02-05. Each record has its announcement and effective-day parameter file;
only close-today is populated because those notices alter only that field. The
CFFEX isolated total is now 762 verified adjustments.

The 2018-2019 audit adds 24 verified close-today adjustments: the 12
then-listed IF/IH/IC contracts changed to `万分之4.6` on 2018-12-03 and to
`万分之3.45` on 2019-04-22. Each change has the official notice and the
effective-day parameter file; open and close-yesterday remain absent because
the notices do not alter them. The CFFEX isolated total is now 786 verified
adjustments.

An archive-wide scan of CFFEX transaction-notice pages 0–105 (2010–2026)
found 12 titles containing `手续费`. Eleven affect futures trading and are now
staged with effective-day parameter evidence; the remaining 2014-12-26 notice
concerns securities-business fees rather than a futures contract and is
intentionally excluded. Later 2020–2026 index pages introduced no additional
fee-titled transaction notice beyond the already reviewed 2023 adjustment.

This is not a continuous CFFEX production history. The candidates must not be
promoted until each contract's actual listing-day rule and every subsequent
official fee change through the promotion boundary have been reconstructed.

## SC/LU coverage boundary

As of the latest local audit, the evidence database has 127 isolated candidate
adjustments for 63 SC/LU contracts, from 2018-03-26 through 2026-05-19. Of
these, 63 have preserved raw bytes for every cited official document and are
`verified`; 64 March 2026 records are `provisional` until their two cited
English notice originals can be retrieved through a normal exchange session.
This includes both products' listing-day fee records, selected 2018 SC
reductions and restorations, a 2021 LU adjustment, and selected 2026 records.
Some adjustments intentionally omit an unstated close-today field. These are
useful reviewed leads, but are not a continuous history from either product's
listing date.

Therefore the earliest safe production-history start date is currently **none**
for both SC and LU. No SC/LU historical interval may be filled or exported
until the complete, contract-specific chain below has been reviewed.

Before a promotion review, retain the exact original bytes under an untracked
directory such as `data/official-evidence/<sha256>.<ext>`. A database record
containing only a URL and digest is not a substitute for the reviewable source
file. If an earlier download is unavailable, retrieve the original again through
an approved human-accessible exchange session, recompute its digest, and stage
the reviewed result again.

### Collection backlog

For SC:

1. Listing-day and selected 2018 adjustments are staged, but most subsequent
   SC fee-adjustment notices are still absent.
2. Collect every missing SC fee-adjustment notice and the corresponding
   effective-day schedule/parameter file, in date order through the first
   already staged 2026 adjustment.
3. For every concrete SC contract, verify its listing date and ensure all three
   fee fields can be reconstructed without borrowing an unstated value from a
   later notice.

For LU:

1. Listing-day and the 2021-04-16 adjustment are staged, but most subsequent
   LU fee-adjustment notices are still absent.
2. Collect every missing LU fee-adjustment notice and its matching official
   schedule/parameter file, in order through the first already staged 2026
   adjustment.
3. Apply the same per-contract, three-field continuity review as SC.

Do not solve a gap with a third-party repost, a search-result snippet, Jin10,
or a 9qihuo single-variety CSV. Exchange WAF pages must be obtained through a
normal human-accessible session; do not bypass or automate challenge solving.

## SHFE discovery boundary

The official SHFE monthly-settlement-parameter directory is human-accessible at
`https://www.shfe.com.cn/reports/businessdata/adjtomonthlysettlementprm/`.
Its 48 static index pages contain 948 dated parameter tables, with the oldest
entries dated August 2005. An inspected 2005-09-23 rubber table explicitly
lists per-contract trading fees alongside effective trading dates, confirming
that this is a usable first-party parameter source rather than a derived
quote page.

No SHFE adjustment has been staged from this discovery alone. Each fee change
still requires retained original bytes and a matching SHFE notice that states
its effective date; table cell styling must be reviewed so ordinary monthly
carry-forward values are not mistaken for changes.

## Promotion rule

Promotion into `fee_versions` is intentionally not implemented. It may happen
only after a reviewer reconstructs all three fields for every affected concrete
contract from a contiguous chain beginning at its listing date. The review must
show that every interval is covered by official documents and that its fee unit
is preserved. Gaps remain gaps; they must never be filled from Jin10, the 9q
single-variety CSV, or a broker/portal repost.

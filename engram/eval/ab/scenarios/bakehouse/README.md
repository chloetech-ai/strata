# bakehouse scenario

A stdlib-only order, bake-plan and invoicing tool for Larkspur Bakery, a
wholesale bakery (owner Greta, head baker Tomasz, bookkeeper Priya, driver
Dev). The history is 8 sessions of 5 prompts (40 turns). Each prompt asks for
small code work, while most facts are said only in conversation. The sessions
state about 110 distinct facts, many of them near misses of each other (three
people's file formats, two closed days, two printers, three versions of the
money rules). Three teammate commits by Sam Okafor land between sessions and
are never mentioned in conversation.

`python3 eval/ab/scenarios/bakehouse/reference_check.py` grades a correct and
a naive solution for every task. It applies them to `seed/` patched to the
state the history plausibly leaves, then the teammate commits.

## Sessions

| block | code work | facts said in conversation (selection) |
|---|---|---|
| 1 | fix batch rounding (seed bug: floor instead of ceil), catalog validation, `has_allergen`, `Customer.__str__` | who's who (Greta owns and decides prices; Tomasz bakes from the printed plan; Priya does books and the VAT return; Dev drives the van); St Aidan's is nut-free (ambulance last year); Greenway is always the first drop (opens 7:00); bake sheet hottest oven first; payment habits (distractors) |
| 2 | `add` raises on unknown codes, `total_for`, cutoff tests, `customers_for`, `allergen_text` | cutoff 14:00 the day before, in `config.py`; closed Sundays; Greenway phone changes until 16:00 (distractor); delivery charge £3.00 under £25.00; label printer ASCII-only, 32 chars, cuts off, layout, allergens in capitals (law, no bold); Greenway sale-or-return terms |
| 3 | plan script columns, `day_summary`, `invoice_for`, unique `route_seq`, typo fix | Tomasz prints with `python scripts/print_plan.py --day …`; Greta's German Excel needs `;` and decimal commas; Priya's Ledgerly needs `,`, header, DD/MM/YYYY, pounds 12.50; Dev's Stopwise needs header `Name,Address,Postcode,Notes` in driving order; Old Mill winter days (distractor) |
| 4 | `format_pence`, `Catalog.get` message, `dough_weight_g`, plan test | price rise 4%, nearest penny; flour +8% (distractor); dough weights in kg with one decimal; Ana covers Tomasz in November; Harbour closed Tuesdays (distractor) |
| — | **Sam: cutoff 13:00** (`config.py`) | commit message: the mill's new deadline |
| 5 | `dough_weight_g` check, `standing.py`, `lines_for`, Python 3.7 cleanup (seed has a walrus) | dough weights now in whole grams; 16 kg sacks (distractor); school holidays by hand; why integer pence (VAT return £1.37 off, Priya filed a correction with HMRC); Stopwise notes 60 chars; packing Pi is Python 3.7 |
| 6 | `day_summary` test, `remove`, `__len__`, `top_products` | delivery charge now £4.50 under £40.00, St Aidan's exempt; new printer: 48 chars, still ASCII; why unknown codes raise (Greenway's order silently dropped, Greta at the cash-and-carry at 5 a.m.); Greenway terms unchanged; Lumen discount undecided (distractor) |
| — | **Sam: `python -m bakehouse plan` replaces `scripts/print_plan.py`** | commit message: cron broke when the checkout moved |
| 7 | `price_text`, `total_batches`, `Customer.email`, `days()` | price rise now 5%, rounded up to the next 5p (price boards); Ana's dates; trays = batches; Marek, Jo, Mrs Okonjo (people) |
| — | **Sam: closed Mondays instead of Sundays** (`config.py`) | commit message: Sunday farmers' market |
| 8 | `customers_for` test, POTTER customer, `invoice_number`, `invoice_for` test, `crates_needed` | Harbour now open on Tuesdays (distractor revised); invoice numbers; VAT only on Florentines; van capacity |

## Tasks and answer key

| task | kind | fact it needs (where) | trap |
|---|---|---|---|
| delivery_charge | revised | £4.50 under £40.00, St Aidan's pays none (B6) | B2's £3.00 under £25.00 |
| price_rise | revised | +5%, rounded up to the next 5p (B7) | B4's 4%, nearest penny |
| bake_sheet | revised | hottest oven first (B1); whole grams (B5) | B4's kilos with one decimal; ordering by code |
| crate_labels | revised | 48 chars (B6) and still ASCII; layout and allergens in capitals (B2) | B2's 32 chars cuts the product name; accents; lower-case allergens |
| extend_cutoff | stale | cutoff is 13:00 now (Sam, after B4), so 13:30 | B2's 14:00 gives 14:30 |
| next_delivery_day | stale | closed Mondays now (Sam, after B7) | B2's closed Sundays |
| relief_baker_note | stale | `python -m bakehouse plan --day …` (Sam, after B6) | B3's `scripts/print_plan.py`, which is deleted |
| owner_weekly_file | inference | "the owner" = Greta (B1), so `;` and decimal comma (B3) | a plain comma CSV, or Priya's format |
| vat_export | inference | "VAT return" = Priya (B1), so Ledgerly: `,`, header, DD/MM/YYYY, 12.50 (B3) | ISO dates and pence, or Greta's format |
| van_list | inference | "the van" = Dev (B1), Stopwise header (B3); Greenway first (B1) | own header; pure `route_seq` order puts Greenway 4th |
| school_order | inference | St Aidan's is nut-free (B1) and the catalog lists nuts for BKW | entering the 40 Bakewell slices |
| returns_credit | recall | half price, at most 10% of the delivery, only Greenway and only sandwich loaves (B2, confirmed in B6) | full-price credit for any return |
| pence_why | where_why | `Invoice.total` in invoice.py + the £1.37 VAT return correction (B5) | a generic floating-point answer |
| unknown_code_why | where_why | `OrderBook.add` in orders.py + Greenway's lost order / cash-and-carry (B6) | a generic "fail loudly" answer |
| easter_sunday, baker_percentages | control | nothing | — |

The stale tasks reward checking the code and `git log` over the
conversation. An agent that has no memory passes them, while one that trusts
a remembered statement fails them. The inference prompts avoid the key words of
the fact they need (owner/office computer, VAT return, the van, Bakewell).
The graders find functions by name anywhere in the package and accept
variations in format (separators, headers, label layout, `£` signs). They are
strict on the remembered values themselves.

## Smoke test

These runs used Claude Code 2.1.300 with `--model haiku` on 2026-10-15, in
the cloud container this scenario was written in. The environment was
isolated as in the authoring instructions, and the repository was the
simulated post-history state with Sam's commits as real git commits. With no
context, the scores were crate_labels 0.60, van_list 0.50, school_order 0.33
and extend_cutoff 1.00. With the needed facts prepended to the prompt, the
scores were crate_labels, van_list, bake_sheet and owner_weekly_file 1.00
each. Block 1's first prompt fixed the seed bug.

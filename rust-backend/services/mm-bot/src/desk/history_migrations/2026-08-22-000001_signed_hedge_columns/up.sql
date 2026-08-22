-- Signed hedge convention (SO-428, doc 08 §4.2): positive = long perp.
-- Legacy rows recorded shorts as positive, so the rename also negates
-- historical values — the stored numbers keep meaning the same position.
-- net_delta_units is unchanged: old net = book_delta − short ≡
-- book_delta + signed_position.
ALTER TABLE desk_symbol_samples RENAME COLUMN hedge_short_units TO hedge_units;
UPDATE desk_symbol_samples SET hedge_units = -hedge_units WHERE hedge_units <> 0;
ALTER TABLE desk_venue_samples RENAME COLUMN short_units TO position_units;
UPDATE desk_venue_samples SET position_units = -position_units WHERE position_units <> 0;

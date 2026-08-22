UPDATE desk_symbol_samples SET hedge_units = -hedge_units WHERE hedge_units <> 0;
ALTER TABLE desk_symbol_samples RENAME COLUMN hedge_units TO hedge_short_units;
UPDATE desk_venue_samples SET position_units = -position_units WHERE position_units <> 0;
ALTER TABLE desk_venue_samples RENAME COLUMN position_units TO short_units;

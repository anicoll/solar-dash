-- The inverter's control settings, saved only when something changes (see
-- `modbus::settings_to_save`), so the rows show when the battery's mode was
-- changed, e.g. by the energy retailer's remote service. A column is NULL
-- when the inverter had never given that value. The EMS mode, forced command
-- and running state are the raw register values.
CREATE TABLE inverter_settings (
  ts                    INTEGER NOT NULL PRIMARY KEY,  -- unix seconds, UTC
  ems_mode              INTEGER,
  forced_command        INTEGER,
  forced_power_w        INTEGER,
  max_soc_pct           REAL,
  min_soc_pct           REAL,
  export_limit_w        INTEGER,
  export_limit_enabled  INTEGER,  -- 0 or 1
  backup_reserve_pct    INTEGER,
  max_charge_w          INTEGER,
  max_discharge_w       INTEGER,
  running_state         INTEGER
);

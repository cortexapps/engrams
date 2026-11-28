-- Publication owns this row. It starts closed and has no published authority.
-- Readers take FOR SHARE before session/ancestor locks. A publisher takes
-- FOR UPDATE before staging a generation or changing participant readiness.
CREATE TABLE managed_operator_generation_gate (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    published_generation BIGINT CHECK (published_generation > 0 AND published_generation <= 9007199254740991),
    published_identity TEXT CHECK (published_identity ~ '^sha256:[0-9a-f]{64}$'),
    admission_open BOOLEAN NOT NULL DEFAULT FALSE,
    CHECK ((published_generation IS NULL) = (published_identity IS NULL)),
    CHECK (NOT admission_open OR published_generation IS NOT NULL)
);
INSERT INTO managed_operator_generation_gate(singleton) VALUES(TRUE);

#!/usr/bin/env bash
# Inserts N synthetic cards directly into search.cards for query benchmarking (bypasses storage).
# Usage: scripts/seed.sh [N=1000000]   then   scripts/bench.sh
set -euo pipefail
N=${1:-1000000}
docker compose exec -T postgres psql -v ON_ERROR_STOP=1 -U postgres -d shepherd <<SQL
\timing on
INSERT INTO search.cards (doc_id, filename, content, fields, status, amount, received_at, facets, schemas,
                          created_at, updated_at, indexed_at)
SELECT gen_random_uuid(),
       'bench-' || g || '.pdf',
       (ARRAY['water damage kitchen ceiling', 'vehicle collision rear bumper', 'roof hail storm shingles',
              'theft of electronics from residence', 'fire damage garage structure'])[1 + g % 5]
         || ' policy HX-' || (g % 100000) || ' contractor estimate drywall repaint remediation',
       jsonb_build_object('intake.ir.v1', jsonb_build_object('status', (ARRAY['open','pending_review','approved','denied'])[1 + g % 4],
                                                             'totals', jsonb_build_object('currency', 'USD'))),
       (ARRAY['open','pending_review','approved','denied'])[1 + g % 4],
       round((random() * 50000)::numeric, 2),
       now() - (g % 365) * interval '1 day',
       ARRAY['type/' || (ARRAY['auto','property','theft','fire'])[1 + g % 4],
             'status/' || (ARRAY['open','pending_review','approved','denied'])[1 + g % 4],
             'schema/intake.ir.v1'],
       ARRAY['intake.ir.v1'],
       now() - (g % 365) * interval '1 day', now() - (g % 365) * interval '1 day', now()
FROM generate_series(1, $N) g;
ANALYZE search.cards;
SQL

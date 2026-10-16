-- Runs once at cluster init (docker-entrypoint-initdb.d) as superuser in database `shepherd`.
-- Development credentials; production roles/passwords are provisioned out of band.

CREATE ROLE storage_rw LOGIN PASSWORD 'storage';
CREATE ROLE search_rw LOGIN PASSWORD 'search';
CREATE ROLE workbench_rw LOGIN PASSWORD 'workbench';
CREATE ROLE notification_rw LOGIN PASSWORD 'notification';
CREATE ROLE inference_rw LOGIN PASSWORD 'inference';

CREATE SCHEMA storage AUTHORIZATION storage_rw;
CREATE SCHEMA search AUTHORIZATION search_rw;
CREATE SCHEMA workbench AUTHORIZATION workbench_rw;
CREATE SCHEMA notification AUTHORIZATION notification_rw;
CREATE SCHEMA inference AUTHORIZATION inference_rw;

-- search reads storage tables directly to build cards.
GRANT USAGE ON SCHEMA storage TO search_rw;
ALTER DEFAULT PRIVILEGES FOR ROLE storage_rw IN SCHEMA storage GRANT SELECT ON TABLES TO search_rw;

-- pg_trgm lives in public so its operators resolve for search_rw.
CREATE EXTENSION IF NOT EXISTS pg_trgm SCHEMA public;

ALTER ROLE storage_rw SET search_path = storage, public;
ALTER ROLE search_rw SET search_path = search, public;
ALTER ROLE workbench_rw SET search_path = workbench, public;
ALTER ROLE notification_rw SET search_path = notification, public;
ALTER ROLE inference_rw SET search_path = inference, public;

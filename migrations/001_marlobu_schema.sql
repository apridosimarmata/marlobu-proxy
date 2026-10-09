-- Marlobu session tracking schema
-- This schema lives in the target database alongside session schemas

CREATE SCHEMA IF NOT EXISTS marlobu;

-- Session registry
CREATE TABLE IF NOT EXISTS marlobu.sessions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id TEXT NOT NULL,
    schema_name TEXT NOT NULL UNIQUE,
    status TEXT NOT NULL DEFAULT 'active',
    created_at TIMESTAMPTZ DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    metadata JSONB,

    CONSTRAINT valid_status CHECK (status IN ('active', 'pending_review', 'approved', 'rejected', 'expired', 'conflict'))
);

-- Track which tables have shadow tables in each session
CREATE TABLE IF NOT EXISTS marlobu.session_tables (
    session_id UUID REFERENCES marlobu.sessions(id) ON DELETE CASCADE,
    table_name TEXT NOT NULL,
    primary_key TEXT NOT NULL DEFAULT 'id',
    shadow_created BOOLEAN DEFAULT false,
    view_created BOOLEAN DEFAULT false,
    PRIMARY KEY (session_id, table_name)
);

-- Indexes for common queries
CREATE INDEX IF NOT EXISTS idx_sessions_status ON marlobu.sessions(status);
CREATE INDEX IF NOT EXISTS idx_sessions_expires ON marlobu.sessions(expires_at);
CREATE INDEX IF NOT EXISTS idx_sessions_project ON marlobu.sessions(project_id);

-- Function to clean up expired sessions
CREATE OR REPLACE FUNCTION marlobu.cleanup_expired_sessions()
RETURNS INTEGER AS $$
DECLARE
    cleaned INTEGER := 0;
    session_record RECORD;
BEGIN
    FOR session_record IN
        SELECT id, schema_name
        FROM marlobu.sessions
        WHERE status = 'active' AND expires_at < now()
    LOOP
        BEGIN
            -- Drop the session schema
            EXECUTE format('DROP SCHEMA IF EXISTS %I CASCADE', session_record.schema_name);

            -- Update session status
            UPDATE marlobu.sessions SET status = 'expired' WHERE id = session_record.id;

            cleaned := cleaned + 1;
        EXCEPTION WHEN OTHERS THEN
            -- Log error but continue
            RAISE WARNING 'Failed to cleanup session %: %', session_record.id, SQLERRM;
        END;
    END LOOP;

    RETURN cleaned;
END;
$$ LANGUAGE plpgsql;

-- Sample data for playground (optional)
-- Uncomment to create test tables

/*
CREATE TABLE IF NOT EXISTS public.users (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    email TEXT UNIQUE NOT NULL,
    role TEXT DEFAULT 'user',
    created_at TIMESTAMPTZ DEFAULT now()
);

CREATE TABLE IF NOT EXISTS public.orders (
    id SERIAL PRIMARY KEY,
    user_id INTEGER REFERENCES users(id),
    status TEXT DEFAULT 'pending',
    total_amount NUMERIC(10, 2),
    created_at TIMESTAMPTZ DEFAULT now()
);

CREATE TABLE IF NOT EXISTS public.inventory (
    id SERIAL PRIMARY KEY,
    product_name TEXT NOT NULL,
    qty INTEGER DEFAULT 0,
    updated_at TIMESTAMPTZ DEFAULT now()
);

INSERT INTO public.users (name, email, role) VALUES
    ('Alice', 'alice@example.com', 'admin'),
    ('Bob', 'bob@example.com', 'user'),
    ('Carol', 'carol@example.com', 'user')
ON CONFLICT (email) DO NOTHING;

INSERT INTO public.orders (user_id, status, total_amount) VALUES
    (1, 'completed', 99.99),
    (2, 'pending', 49.50),
    (2, 'completed', 199.00)
ON CONFLICT DO NOTHING;

INSERT INTO public.inventory (product_name, qty) VALUES
    ('Widget A', 100),
    ('Widget B', 50),
    ('Gadget X', 25)
ON CONFLICT DO NOTHING;
*/

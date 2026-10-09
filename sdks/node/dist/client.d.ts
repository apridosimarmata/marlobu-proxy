import { Session } from './session.js';
import type { MarlobuConfig, CreateSessionOptions } from './types.js';
/**
 * Main client for interacting with the marlobu-proxy service.
 */
export declare class Marlobu {
    private readonly apiUrl;
    private readonly proxyHost;
    private readonly proxyPort;
    /**
     * Create a new Marlobu client.
     *
     * @param config - Client configuration
     * @param config.apiUrl - Base URL for the marlobu-proxy API
     * @param config.proxyHost - Hostname of the PostgreSQL proxy (default: 'localhost')
     * @param config.proxyPort - Port of the PostgreSQL proxy (default: 5433)
     */
    constructor(config: MarlobuConfig);
    /**
     * Create a new session for staging database changes.
     *
     * @param options - Session creation options
     * @param options.projectId - Project identifier for the session
     * @returns A new Session instance
     */
    createSession(options: CreateSessionOptions): Promise<Session>;
    /**
     * Get an existing session by ID.
     *
     * @param sessionId - The session ID to retrieve
     * @returns The Session instance
     */
    getSession(sessionId: string): Promise<Session>;
}
//# sourceMappingURL=client.d.ts.map
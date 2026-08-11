// Thin wrapper around a libsoup session for talking to render-server's
// local HTTP API (see crates/render-server/src/main.rs's module doc
// comment for the full contract) -- GJS has no XMLHttpRequest outside a
// WebKit context, unlike the Plasma wallpaper plugin's QML, so this is
// the GNOME-side equivalent of what main.qml's inline XHR calls do.

import GLib from 'gi://GLib';
import Soup from 'gi://Soup?version=3.0';

const BASE_URL = 'http://127.0.0.1:47824';

export class RenderServerClient {
    constructor() {
        this._session = new Soup.Session();
        // Local-only traffic to a process we just spawned ourselves --
        // no reason to ever wait longer than this for a reply.
        this._session.timeout = 2;
    }

    destroy() {
        this._session?.abort();
        this._session = null;
    }

    /**
     * GET `path` (already including any query string) and resolve with
     * the response body as `GLib.Bytes`, or `null` on any failure
     * (connection refused, non-200 status, cancellation). Never rejects
     * -- every caller here treats "couldn't fetch" as "try again next
     * poll", same as main.qml's XHR error handling.
     */
    getBytes(path, cancellable = null) {
        if (!this._session)
            return Promise.resolve(null);

        const message = Soup.Message.new('GET', `${BASE_URL}${path}`);
        if (!message)
            return Promise.resolve(null);

        return new Promise(resolve => {
            this._session.send_and_read_async(
                message, GLib.PRIORITY_DEFAULT, cancellable,
                (session, result) => {
                    try {
                        const bytes = session.send_and_read_finish(result);
                        if (message.get_status() !== Soup.Status.OK) {
                            resolve(null);
                            return;
                        }
                        resolve(bytes);
                    } catch (e) {
                        // Includes Gio.IOErrorEnum.CANCELLED -- callers
                        // that pass a cancellable already know to ignore
                        // a `null` result in that case.
                        resolve(null);
                    }
                });
        });
    }

    async getJson(path, cancellable = null) {
        const bytes = await this.getBytes(path, cancellable);
        if (!bytes)
            return null;
        try {
            return JSON.parse(new TextDecoder().decode(bytes.get_data()));
        } catch (e) {
            return null;
        }
    }

    /** POST `path` with a plain-text `body`. Fire-and-forget: resolves once the request settles, result is never checked by callers (matches main.qml's `xhr.send()` for /geometry, which never reads the response either). */
    postText(path, body, cancellable = null) {
        if (!this._session)
            return Promise.resolve();

        const message = Soup.Message.new('POST', `${BASE_URL}${path}`);
        if (!message)
            return Promise.resolve();
        message.set_request_body_from_bytes(
            'text/plain', GLib.Bytes.new(new TextEncoder().encode(body)));

        return new Promise(resolve => {
            this._session.send_and_read_async(
                message, GLib.PRIORITY_DEFAULT, cancellable,
                (session, result) => {
                    try {
                        session.send_and_read_finish(result);
                    } catch (e) {
                        // best-effort, see class doc comment
                    }
                    resolve();
                });
        });
    }
}

// Synthetic callback shape, not live OAuth.
import { postLogin } from "../../../../../lib/auth/post-login";
import { localePortal } from "../../../../../lib/auth/redirects";
export function exchangeCode(code: string) { return { fixtureOnly: true, code }; }
export function callback(code: string, locale: string) {
  const session = exchangeCode(code);
  postLogin(null, null);
  return { session, destination: localePortal(locale) };
}

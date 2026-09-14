// Synthetic account-rule evidence. Never grants merchant approval.
export function phoneMatch(phone: string) { return { match: phone, merchantApproved: false }; }
export function emailMatch(email: string) { return { match: email, merchantApproved: false }; }
export function postLogin(phone: string | null, email: string | null) {
  if (phone) return phoneMatch(phone);
  if (email) return emailMatch(email);
  return { match: null, merchantApproved: false };
}

// Public pages and owner pages of one app. A view's plan entry may say who
// it is for: everyone (the default), "members" (signed in) or "owner" (the
// owner and admins: managing the rows a public page shows, settings). A
// visitor of a published app, and the studio's "See it as a stranger", get
// the app without its owner's views, so its menu never offers them editing
// tools. What a view may read and write is still the tables' access.

const AUDIENCES = ['members', 'owner'];

// The spec's "for" of a view from its plan entry; a view rewritten without
// saying keeps what it had (view: the app as it is).
export function audienceOf(item, view) {
  const said = typeof item?.for === 'string' ? item.for.toLowerCase().trim() : null;
  const given = said === 'admins' || said === 'admin' ? 'owner' : said === 'member' ? 'members' : said;
  if (given) return AUDIENCES.includes(given) ? { for: given } : {};
  const had = view?.views?.find((v) => v.slug === item?.slug)?.spec?.for;
  return AUDIENCES.includes(had) ? { for: had } : {};
}

// The views a visitor of the app gets.
export const forVisitors = (views) => (views || []).filter((c) => c?.spec?.for !== 'owner');

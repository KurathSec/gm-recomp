// The page's files (runtime, importer, mods, profiles, system DLLs, games) are fetched
// relative to these modules' directory, so the page works wherever it is served:
// at / on the dev server, under /play/ on a site that embeds it.
export const asset = (path) => new URL(`../${path}`, import.meta.url);

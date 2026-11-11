# Changelog

## 0.2.0

`toaba assets sync` and `toaba assets branch sync` keep the files that are in the bucket but not in
the folder. Pass `--delete` to remove them, so the bucket matches the folder exactly, as 0.1.0 always
did.

A Unity Addressables content update needs this. In groups with **Prevent Updates**, the new catalog
still loads unchanged assets from the earlier build's bundles, and the update build doesn't write
those bundles again. Synced from a clean folder, as in CI, 0.1.0 removed them, and the new release
was missing bundles its catalog needs.

The plan counts the files that stay, such as `2 only in the bucket`, and `-o json` lists them under
`kept`.

## 0.1.0

The first release: log in, choose an app, sync a folder into a bucket, cut releases, point tags at
them, promote a release from one bucket to another, and give each branch or pull request its own
bucket.

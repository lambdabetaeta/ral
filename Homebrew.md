# Homebrew release process

The formula lives at `HomebrewFormula/ral.rb` in this repo.
Users install via:

```
brew install lambdabetaeta/ral/ral
```

Homebrew auto-taps `https://github.com/lambdabetaeta/ral` on first install.

## Cutting a release

1. **Tag and push.**

   ```
   git tag v0.X.Y
   git push all v0.X.Y
   ```

2. **Create a GitHub release** from the tag (GitHub → Releases → Draft new
   release → choose the tag → Publish). This generates the source tarball at:

   ```
   https://github.com/lambdabetaeta/ral/archive/refs/tags/v0.X.Y.tar.gz
   ```

3. **Compute the SHA256** of the tarball.

   ```
   curl -sL https://github.com/lambdabetaeta/ral/archive/refs/tags/v0.X.Y.tar.gz \
     | shasum -a 256
   ```

4. **Update `HomebrewFormula/ral.rb`** — replace the `url` and `sha256` lines:

   ```ruby
   url "https://github.com/lambdabetaeta/ral/archive/refs/tags/v0.X.Y.tar.gz"
   sha256 "<output from step 3, first field>"
   ```

5. **Commit and push.**

   ```
   git add HomebrewFormula/ral.rb
   git commit -m "chore: bump formula to v0.X.Y"
   git push all main
   ```

Users running `brew upgrade ral` will pick up the new version automatically.

## Local testing before publishing

```
brew install --build-from-source ./HomebrewFormula/ral.rb
brew test ral
```

## Installing from HEAD (development builds)

```
brew install --HEAD lambdabetaeta/ral/ral
```

This builds directly from the `main` branch; the sha256 is not checked.

## Registering ral-sh as a login shell (post-install)

```
sudo sh -c 'echo $(brew --prefix)/bin/ral-sh >> /etc/shells'
chsh -s $(brew --prefix)/bin/ral-sh
```

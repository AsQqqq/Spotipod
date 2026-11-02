.PHONY: app dmg

app:
	@sh scripts/build-macos-app.sh

dmg:
	@sh scripts/build-macos-dmg.sh

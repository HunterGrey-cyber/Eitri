These SVGs come from the installed Mermaid 11.17.2 renderer. Run `node generate.mjs` in this directory
to regenerate them using the Markdown plugin's existing dependencies. The generator supplies jsdom SVG text
geometry because these fixtures exercise CSS retention and theming, not layout or appearance. The base theme
uses `#123456` for surfaces and `#abcdef` for text and lines. Sources are a two-node flowchart, an Alice-to-Bob
sequence and an Animal/Duck class relationship; the generator contains their complete inputs and configuration.

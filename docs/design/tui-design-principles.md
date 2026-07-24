## Design Context

### Users

Developers and terminal-heavy users who repeatedly move between many working directories. Their primary job is to find and jump to the right directory with minimal keystrokes while retaining enough context to distinguish similarly named paths.

### Brand Personality

Calm, precise, and fast. The interface should feel dependable under repeated daily use, with concise localized copy and no decorative theatrics.

### Aesthetic Direction

A refined utilitarian terminal interface: dense but breathable, keyboard-first, and compatible with both colored and color-disabled terminals. Visual hierarchy comes from restrained contrast, weight, alignment, and progressive disclosure rather than logos, score graphics, cards, gradients, or animation.

### Design Principles

1. Keep search and the selected directory as the strongest visual signals.
2. Show only information that helps users choose or recover; reveal preview and help on demand.
3. Preserve a stable layout across filtering, paging, resize, loading, and error states.
4. Treat Unicode display width, narrow terminals, and color-disabled mode as first-class cases.
5. Prefer immediate event-driven feedback and explicit recovery paths over motion or timed behavior.

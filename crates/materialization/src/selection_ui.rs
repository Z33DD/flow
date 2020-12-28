use crate::Error;
use itertools::Itertools;
use protocol::flow::{CollectionSpec, Projection};
use skim::{
    AnsiString, DisplayContext, ItemPreview, PreviewContext, Selector, Skim, SkimItem, SkimOptions,
};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use tuikit::attr::{Attr, Effect};

/// Provides the `.width()` function on strings to calculate the rendered width of strings.
/// We use this to determine padding for text alignment.
use unicode_width::UnicodeWidthStr;

/// Used to mark projections that are part of the collection's key
const KEY_MARKER: &str = "\u{1F511}";

/// Can be re-used if we add a selection UI for other things like collections or targets
const GENERIC_INSTRUCTIONS: &str = "Use arrow keys, search, or mouse to select fields.\nPress tab or space, or right-click with the mouse to toggle fields on or off.\nPress enter when done or escape to cancel.";

/// Runs the interactive field selection UI, and blocks until the user has either accepted a set of
/// projections or cancelled. If the user cancels or aborts, an `Error::ActionAborted` is returned.
/// The list of returned projections is guaranteed to be valid for a materialization. If the user
/// selects an invalid set of projections (missing a collection key component), then an
/// `Error::MissingCollectionKeys` is returned.
pub fn interactive_select_projections(
    collection: CollectionSpec,
) -> Result<Vec<Projection>, Error> {
    let header = field_selection_header(&collection);
    let opts = projection_skim_options(&collection.projections, &header);

    // determine the max length of a field name. We'll use this to calculate padding so that
    // columns line up nicely
    let max_length = collection
        .projections
        .iter()
        .map(|proj| proj.field.width())
        .max()
        .expect("Empty list of projections. This is a bug.");

    let context = Arc::new(FieldSelectionContext {
        collection,
        max_rendered_field_length: max_length,
    });
    let (tx, rx) = skim::prelude::unbounded::<Arc<dyn SkimItem>>();
    for field_index in 0..context.collection.projections.len() {
        let projection = SkimProjection {
            field_index,
            context: context.clone(),
        };
        tx.send(Arc::new(projection)).unwrap();
    }

    // This call will block until the user either accepts the selection or cancels. Will return
    // None if there's an "internal error" from skim. We'll surface this as an i/o error for now,
    // since I have no idea how common this is or what might cause it. It's important that we don't
    // use stdin, stdout, or stderr for anything else while skim is doing its thing. Specifically,
    // this means don't try to log anything on calls to our `SkimProjection`.
    let output = Skim::run_with(&opts, Some(rx)).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::Other, "interactive selection UI error")
    })?;
    // We do this just to ensure that anything written by skim is finished and handled by the
    // terminal before we resume our normal terminal usage for printing logs and such.
    flush_std_streams();

    log::info!(
        "Finished selection with end event: {:?}, query: {:?}, cmd: {:?}",
        output.final_event,
        output.query,
        output.cmd
    );
    if output.is_abort {
        return Err(Error::ActionAborted);
    }

    let mut results = output
        .selected_items
        .into_iter()
        .map(|selection| {
            selection
                .as_any()
                .downcast_ref::<SkimProjection>()
                .expect("unexpected item returned from skim")
                .projection()
                .clone()
        })
        .collect::<Vec<_>>();

    // Ensures that the user has selected a valid subset of fields that includes all components of
    // the collection's key
    super::validate_projected_fields(&context.collection, results.as_slice())?;

    // Re-order the projections to put all projections that are part of the key at the beginning.
    // This is purely to make the resulting sql more readable.
    results.sort_by_key(|p| !p.is_primary_key);
    Ok(results)
}

fn flush_std_streams() {
    use std::io::Write;

    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
}

struct FieldSelectionContext {
    collection: CollectionSpec,
    max_rendered_field_length: usize,
}

struct SkimProjection {
    context: Arc<FieldSelectionContext>,
    field_index: usize,
}
impl SkimProjection {
    fn projection(&self) -> &Projection {
        &self.context.collection.projections[self.field_index]
    }
}

impl SkimItem for SkimProjection {
    fn text(&self) -> Cow<str> {
        // We use ONLY the field name as `text`, which means that skim will only match against that
        // when the user searches. This is to prvent matching against text that's just part of the
        // description.
        Cow::Borrowed(self.projection().field.as_str())
    }

    fn display<'a>(&self, ctx: DisplayContext<'a>) -> AnsiString {
        let types = self
            .projection()
            .inference
            .as_ref()
            .map(|i| i.types.as_slice())
            .unwrap_or_default()
            .iter()
            .join(", ");
        let padding = {
            let field_len = self.projection().field.width();
            let space_count = self.context.max_rendered_field_length - field_len;
            std::iter::repeat(" ").take(space_count).join("")
        };
        let mut s = format!("{}{}\t[{}]", self.projection().field, padding, types);
        let attrs = if self.projection().is_primary_key {
            let range_end = s.len() as u32;
            s.push(' ');
            s.push_str(KEY_MARKER);
            vec![(
                Attr {
                    fg: ctx.highlight_attr.fg,
                    bg: ctx.highlight_attr.bg,
                    effect: Effect::BOLD,
                },
                (0u32, range_end),
            )]
        } else {
            Vec::new()
        };

        AnsiString::new_string(s, attrs)
    }

    fn preview(&self, context: PreviewContext) -> ItemPreview {
        let projection = ProjectionPreview(self.projection());

        // Below the field info, we'll show a list of all the currently selected fields.
        let all_selected = context.selections.iter().join("\n\t");

        let preview = format!(
            "Selected Projection:\n\n{}\nAll Selected Fields:\n\t{}",
            projection, all_selected
        );
        ItemPreview::Text(preview)
    }
}

struct ProjectionPreview<'a>(&'a Projection);
impl<'a> fmt::Display for ProjectionPreview<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "\tField:        {}\n\
                \tJSON Pointer: {}\n",
            self.0.field, self.0.ptr
        )?;

        if let Some(inference) = self.0.inference.as_ref() {
            writeln!(f, "\tType:         {}\n\
                  \tTitle:        {}\n\
                  \tDescription:  {}\n\
                  \tMust Exist: {} (if true, then the field can never be 'undefined', though it can be null)",
                  inference.types.iter().join(", "),
                  inference.title,
                  inference.description,
                  inference.must_exist)?;
        } else {
            writeln!(
                f,
                "\tError: No type inference information available for this location."
            )?;
        }
        let source_description = if self.0.user_provided {
            "User-provided projection"
        } else {
            "Automatically generated projection"
        };
        let key_comment = if self.0.is_primary_key {
            "\tKey: \u{1F511} This location is part of the Collection's key.\n"
        } else {
            ""
        };
        let partition_comment = if self.0.is_partition_key {
            "\tPartition Key: This location is used as a partition key.\n"
        } else {
            ""
        };

        write!(
            f,
            "\tSource: {}\n{}{}",
            source_description, key_comment, partition_comment
        )
    }
}

fn field_selection_header(collection: &CollectionSpec) -> String {
    format!(
        "Please select the fields to materialize.\n\
        {}\n\n\
        Showing all projections for collection '{}' that match the search.\n\
        {} indicates fields that are part of the collection's key: [{}].\n",
        GENERIC_INSTRUCTIONS,
        collection.name,
        KEY_MARKER,
        collection.key_ptrs.iter().join(", ")
    )
}

fn projection_skim_options<'a>(fields: &[Projection], header: &'a str) -> SkimOptions<'a> {
    use std::rc::Rc;
    let default_selector = DefaultPreSelector::from_fields(fields);
    SkimOptions {
        multi: true,
        nosort: true,
        layout: "reverse",
        prompt: Some("search > "),
        header: Some(header),
        bind: vec![
            "tab:toggle+unix-line-discard",
            "space:toggle",
            "alt-p:toggle-preview",
        ],
        selector: Some(Rc::new(default_selector)),
        // We don't use the global preview command, but the preview window won't be shown if this is None
        preview: Some(""),
        preview_window: Some("right:50%:wrap"),

        // Start with the builtin "dark" color scheme, then set the colors of the markers to make
        // them a little more obvious.
        color: Some("dark,selected:9,pointer:15"),
        ..Default::default()
    }
}

/// Implements skim::Selector, which determines the set of items that will be selected by default.
/// When we display the UI, these items will already be selected. We do this for pointers used as
/// collection keys, and any projections that were user-provided. This function will deduplicate
/// projections by json pointer. Preference is always given to user-provided projections over those
/// that were generated automatically.
struct DefaultPreSelector(HashSet<String>);
impl DefaultPreSelector {
    fn from_fields(projections: &[Projection]) -> DefaultPreSelector {
        let mut by_location = HashMap::with_capacity(8);

        // First add all user-provided projections. In the case that there are multiple
        // user-provided projections for a given location, we'll just pick one arbitrarily based on
        // insertion order.
        for projection in projections.iter().filter(|p| p.user_provided) {
            by_location.insert(projection.ptr.as_str(), projection);
        }

        // Now add all key projections, but only if they're not already represented.
        for projection in projections.iter().filter(|f| f.is_primary_key) {
            if !by_location.contains_key(&projection.ptr.as_str()) {
                by_location.insert(projection.ptr.as_str(), projection);
            }
        }
        let default_fields: HashSet<String> = by_location
            .into_iter()
            .map(|(_, projection)| projection.field.clone())
            .collect();
        DefaultPreSelector(default_fields)
    }
}
impl Selector for DefaultPreSelector {
    fn should_select(&self, _index: usize, item: &dyn SkimItem) -> bool {
        let field: &str = item
            .as_any()
            .downcast_ref::<SkimProjection>()
            .unwrap()
            .projection()
            .field
            .as_str();
        self.0.contains(field)
    }
}
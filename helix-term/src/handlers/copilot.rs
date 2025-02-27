use std::time::Duration;
use helix_core::{coords_at_pos, softwrapped_dimensions, Rope};
use helix_event::{
    cancelable_future, register_hook, send_blocking, TaskController, TaskHandle,
};
use helix_lsp::copilot_types::DocCompletion;
use helix_lsp::util::{lsp_pos_to_pos, lsp_range_to_range};
use helix_view::document::Mode;
use helix_view::copilot::CopilotStatus;
use helix_view::Editor;
use helix_view::events::DocumentDidChange;
use tokio::time::Instant;
use crate::compositor::Compositor;
use crate::events::OnModeSwitch;
use crate::handlers::Handlers;
use crate::ui;
use helix_view::handlers::lsp::CopilotRequestCompletionEvent;
use crate::job::{dispatch, dispatch_blocking};

pub struct CopilotHandler {
    task_controller: TaskController,
}

impl CopilotHandler {
    pub fn new() -> Self {
        Self {
            task_controller: TaskController::new(),
        }
    }
}

impl helix_event::AsyncHook for CopilotHandler {
    type Event = CopilotRequestCompletionEvent;
    fn handle_event(
        &mut self,
        _: Self::Event,
        _: Option<Instant>,
    ) -> Option<Instant> {
        Some(Instant::now() + Duration::from_millis(100))
    }

    fn finish_debounce(&mut self) {
        let handle = self.task_controller.restart();

        dispatch_blocking(move |editor, compositor| {
            copilot_completion(editor, compositor, handle);
        });
    }
}

fn copilot_completion(editor: &mut Editor, compositor: &mut Compositor, handle: TaskHandle) {
    let (view, doc) = current_ref!(editor);
    // check editor mode since we request a completion on DocumentDidChange even when not in Insert Mode  
    // (this cannot be checked within try_register_hooks unforunately)
    // (the completion will not render, but there is still not point sending the request to the copilot lsp)
    if editor.mode() != Mode::Insert { return; }

    let Some(copilot_ls) = doc
        .language_servers()
        .filter(|ls| ls.name() == "copilot")
        .next()
    else { return; };

    let copilot_id = copilot_ls.id();

    let editor_view = compositor.find::<ui::EditorView>().unwrap();
    let spinner = editor_view.spinners_mut().get_or_create(copilot_id);
    spinner.start();

    let offset_encoding = copilot_ls.offset_encoding();
    let copilot_future = if let Some(copilot_doc) = doc.copilot_document(view.id, offset_encoding) {
        copilot_ls.copilot_completion(copilot_doc)
    } else {
        return;
    };
   
    let (_, doc) = current!(editor);
    doc.copilot.set_status(CopilotStatus::Fetching);

    tokio::spawn(async move {
        if let Some(item) = cancelable_future(copilot_future, handle).await {
            if let Ok(Some(completion_reponse)) = item {
                dispatch(move |editor, compositor| {
                    let (view, doc) = current!(editor);

                    let editor_view = compositor.find::<ui::EditorView>().unwrap();
                    let spinner = editor_view.spinners_mut().get_or_create(copilot_id);
                    spinner.stop();
                    
                    doc.copilot.set_status(CopilotStatus::Success(completion_reponse.completions.len()));
                    let completions = if completion_reponse.completions.len() > 0 {
                        completion_reponse.completions
                    } else {
                        return;
                    };

                    let doc_completions = completions
                        .into_iter()
                        .filter_map(|completion| {
                            /*
                            NOTE:
                            The computation below is neccesary because:

                            1. If we're typing the string ' let x = Vec::From(*);' where * is cursor position, copilot will sometimes respond with the
                            completion '[1,2,3]' where '[1,2,3]' is to be inserted between the two brackets. It will not give any other information about 
                            the first line. In this case, we cannot only render the virtual text '[1,2,3]' (rendering virtual text has no effect on the position 
                            of the doc's non-virtual text (it will not cause ');' to move rightwards).
                            Hence we need to calculate what the whole first line will look like post applying copilot's completion.

                            2. We also need to calculate what the whole first line will look like post applying the completion in order to calculate the number of 
                            additional lines needed for first line's softwrap. Eg ' let x = String::From(*);' may not require softwrapping, but 
                            ' let x = String::From([1,2,3]);'  may do. 

                            The remaining additional lines that copilot may insert will not be interleaved with the doc's text, so the above problems are only relevant 
                            to the completion's first line
                            */
                            let txt_fmt = doc.text_format(view.inner_width(&doc), None);

                            let Some(range) = lsp_range_to_range(doc.text(), completion.range, offset_encoding) else {return None;};
                            let (start_coords, end_coords) = (coords_at_pos(doc.text().slice(..).into(), range.anchor), coords_at_pos(doc.text().slice(..).into(), range.head));

                            let display_pos =
                                lsp_pos_to_pos(doc.text(), completion.position, offset_encoding)?;
                            let display_coords =
                                coords_at_pos(doc.text().slice(..).into(), display_pos);

                            let line_idx = doc.text().char_to_line(display_pos);
                            let mut line_rope = Rope::from(doc.text().get_line(line_idx)?);
                            let pre_insert_softwrap =
                                softwrapped_dimensions(line_rope.slice(..), &txt_fmt).0;

                            line_rope.remove(start_coords.col..end_coords.col);
                            line_rope.insert(start_coords.col, &completion.text);
                            let post_insert_softwrap =
                                softwrapped_dimensions(line_rope.slice(..), &txt_fmt).0;
                            
                            Some(DocCompletion {
                                text: completion.text,
                                lsp_range: completion.range,
                                display_text: line_rope.to_string(),
                                display_coords,
                                additional_softwrap: post_insert_softwrap - pre_insert_softwrap,
                                doc_version: doc.version() as usize,
                            })
                        })
                        .collect::<Vec<DocCompletion>>();

                    doc.copilot.fill_with_completions(doc_completions, offset_encoding);
                })
                .await;
            }
        }
    });
}

pub(super) fn try_register_hooks(handlers: &Handlers) {
    let Some(copilot_handler) = handlers.copilot.clone() else {return;};

    let tx = copilot_handler.clone();
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        event.doc.copilot.delete_state_and_reset_should_render();
        send_blocking(&tx, CopilotRequestCompletionEvent);
        Ok(())
    });

    let tx = copilot_handler.clone();
    register_hook!(move |event: &mut OnModeSwitch<'_, '_>| {
        let (_, doc) = current!(event.cx.editor);

        doc.copilot.reset_status();
        if event.old_mode == Mode::Insert {
            doc.copilot.delete_state_and_should_not_render();
        } else if event.new_mode == Mode::Insert {
            doc.copilot.delete_state_and_reset_should_render();
            send_blocking(&tx, CopilotRequestCompletionEvent);
        }
        Ok(())
    });
}
